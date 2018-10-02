use support::*;

use std::sync::{Arc, Mutex};

use convert::TryFrom;

pub fn new() -> Proxy {
    Proxy::new()
}

pub struct Proxy {
    controller: Option<controller::Listening>,
    inbound: Option<server::Listening>,
    outbound: Option<server::Listening>,

    inbound_disable_ports_protocol_detection: Option<Vec<u16>>,
    outbound_disable_ports_protocol_detection: Option<Vec<u16>>,

    shutdown_signal: Option<Box<Future<Item=(), Error=()> + Send>>,
}

pub struct Listening {
    pub control: SocketAddr,
    pub inbound: SocketAddr,
    pub outbound: SocketAddr,
    pub metrics: SocketAddr,

    pub outbound_server: Option<server::Listening>,
    pub inbound_server: Option<server::Listening>,

    shutdown: Shutdown,
}

impl Proxy {
    pub fn new() -> Self {
        Proxy {
            controller: None,
            inbound: None,
            outbound: None,

            inbound_disable_ports_protocol_detection: None,
            outbound_disable_ports_protocol_detection: None,
            shutdown_signal: None,
        }
    }

    /// Pass a customized support `Controller` for this proxy to use.
    ///
    /// If not used, a default controller will be used.
    pub fn controller(mut self, c: controller::Listening) -> Self {
        self.controller = Some(c);
        self
    }

    pub fn inbound(mut self, s: server::Listening) -> Self {
        self.inbound = Some(s);
        self
    }

    pub fn outbound(mut self, s: server::Listening) -> Self {
        self.outbound = Some(s);
        self
    }

    pub fn disable_inbound_ports_protocol_detection(mut self, ports: Vec<u16>) -> Self {
        self.inbound_disable_ports_protocol_detection = Some(ports);
        self
    }

    pub fn disable_outbound_ports_protocol_detection(mut self, ports: Vec<u16>) -> Self {
        self.outbound_disable_ports_protocol_detection = Some(ports);
        self
    }

    pub fn shutdown_signal<F>(mut self, sig: F) -> Self
    where
        F: Future + Send + 'static,
    {
        // It doesn't matter what kind of future you give us,
        // we'll just wrap it up in a box and trigger when
        // it triggers. The results are discarded.
        let fut = Box::new(sig.then(|_| Ok(())));
        self.shutdown_signal = Some(fut);
        self
    }

    pub fn run(self) -> Listening {
        self.run_with_test_env(config::TestEnv::new())
    }

    pub fn run_with_test_env(self, env: config::TestEnv) -> Listening {
        run(self, env)
    }
}

#[derive(Clone, Debug)]
struct MockOriginalDst(Arc<Mutex<DstInner>>);

#[derive(Debug, Default)]
struct DstInner {
    inbound_orig_addr: Option<SocketAddr>,
    inbound_local_addr: Option<SocketAddr>,
    outbound_orig_addr: Option<SocketAddr>,
    outbound_local_addr: Option<SocketAddr>,
}

impl linkerd2_proxy::GetOriginalDst for MockOriginalDst {
    fn get_original_dst(&self, sock: &AddrInfo) -> Option<SocketAddr> {
        sock.local_addr()
            .ok()
            .and_then(|local| {
                let inner = self.0.lock().unwrap();
                if inner.inbound_local_addr == Some(local) {
                    inner.inbound_orig_addr
                } else if inner.outbound_local_addr == Some(local) {
                    inner.outbound_orig_addr
                } else {
                    None
                }
            })
    }
}


fn run(proxy: Proxy, mut env: config::TestEnv) -> Listening {
    use self::linkerd2_proxy::config;

    let controller = proxy.controller.unwrap_or_else(|| controller::new().run());
    let inbound = proxy.inbound;
    let outbound = proxy.outbound;
    let mut mock_orig_dst = DstInner::default();

    env.put(config::ENV_CONTROL_URL, format!("tcp://{}", controller.addr));
    env.put(config::ENV_OUTBOUND_LISTENER, "tcp://127.0.0.1:0".to_owned());
    if let Some(ref inbound) = inbound {
        env.put(config::ENV_INBOUND_FORWARD, format!("tcp://{}", inbound.addr));
        mock_orig_dst.inbound_orig_addr = Some(inbound.addr);
    }
    if let Some(ref outbound) = outbound {
        mock_orig_dst.outbound_orig_addr = Some(outbound.addr);
    }
    env.put(config::ENV_INBOUND_LISTENER, "tcp://127.0.0.1:0".to_owned());
    env.put(config::ENV_CONTROL_LISTENER, "tcp://127.0.0.1:0".to_owned());
    env.put(config::ENV_METRICS_LISTENER, "tcp://127.0.0.1:0".to_owned());
    env.put(config::ENV_POD_NAMESPACE, "test".to_owned());

    if let Some(ports) = proxy.inbound_disable_ports_protocol_detection {
        let ports = ports.into_iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(",");
        env.put(
            config::ENV_INBOUND_PORTS_DISABLE_PROTOCOL_DETECTION,
            ports
        );
    }

    if let Some(ports) = proxy.outbound_disable_ports_protocol_detection {
        let ports = ports.into_iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(",");
        env.put(
            config::ENV_OUTBOUND_PORTS_DISABLE_PROTOCOL_DETECTION,
            ports
        );
    }

    let config = config::Config::try_from(&env).unwrap();

    let (running_tx, running_rx) = oneshot::channel();
    let (tx, mut rx) = shutdown_signal();

    if let Some(fut) = proxy.shutdown_signal {
        rx = Box::new(rx.select(fut).then(|_| Ok(())));
    }

    let tname = format!("support proxy (test={})", thread_name());
    ::std::thread::Builder::new()
        .name(tname)
        .spawn(move || {
            let _c = controller;

            let mock_orig_dst = MockOriginalDst(Arc::new(Mutex::new(mock_orig_dst)));
            // TODO: a mock timer could be injected here?
            let runtime = tokio::runtime::current_thread::Runtime::new()
                .expect("initialize main runtime");
            let main = linkerd2_proxy::Main::new(
                config,
                mock_orig_dst.clone(),
                runtime,
            );

            let control_addr = main.control_addr();
            let inbound_addr = main.inbound_addr();
            let outbound_addr = main.outbound_addr();
            let metrics_addr = main.metrics_addr();

            {
                let mut inner = mock_orig_dst.0.lock().unwrap();
                inner.inbound_local_addr = Some(inbound_addr);
                inner.outbound_local_addr = Some(outbound_addr);
            }

            // slip the running tx into the shutdown future, since the first time
            // the shutdown future is polled, that means all of the proxy is now
            // running.
            let addrs = (
                control_addr,
                inbound_addr,
                outbound_addr,
                metrics_addr,
            );
            let mut running = Some((running_tx, addrs));
            let on_shutdown = future::poll_fn(move || {
                if let Some((tx, addrs)) = running.take() {
                    let _ = tx.send(addrs);
                }

                rx.poll()
            });

            main.run_until(on_shutdown);
        })
        .unwrap();

    let (control_addr, inbound_addr, outbound_addr, metrics_addr) =
        running_rx.wait().unwrap();

    // printlns will show if the test fails...
    println!(
        "proxy running; control={}, inbound={}{}, outbound={}{}, metrics={}",
        control_addr,
        inbound_addr,
        inbound
            .as_ref()
            .map(|i| format!(" (SO_ORIGINAL_DST={})", i.addr))
            .unwrap_or_else(String::new),
        outbound_addr,
        outbound
            .as_ref()
            .map(|o| format!(" (SO_ORIGINAL_DST={})", o.addr))
            .unwrap_or_else(String::new),
        metrics_addr,
    );

    Listening {
        control: control_addr,
        inbound: inbound_addr,
        outbound: outbound_addr,
        metrics: metrics_addr,

        outbound_server: outbound,
        inbound_server: inbound,

        shutdown: tx,
    }
}
