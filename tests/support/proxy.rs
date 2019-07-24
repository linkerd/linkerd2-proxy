use support::*;

use std::sync::{Arc, Mutex};

pub fn new() -> Proxy {
    Proxy::new()
}

pub struct Proxy {
    controller: Option<controller::Listening>,
    identity: Option<controller::Listening>,

    inbound: Option<SocketAddr>,
    outbound: Option<SocketAddr>,

    inbound_server: Option<server::Listening>,
    outbound_server: Option<server::Listening>,

    inbound_disable_ports_protocol_detection: Option<Vec<u16>>,
    outbound_disable_ports_protocol_detection: Option<Vec<u16>>,

    shutdown_signal: Option<Box<Future<Item = (), Error = ()> + Send>>,
}

pub struct Listening {
    pub control: Option<SocketAddr>,
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
            identity: None,

            inbound: None,
            outbound: None,

            inbound_server: None,
            outbound_server: None,

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

    pub fn identity(mut self, i: controller::Listening) -> Self {
        self.identity = Some(i);
        self
    }

    pub fn inbound(mut self, s: server::Listening) -> Self {
        let addr = s.addr.clone();
        self.inbound = Some(addr);
        self.inbound_server = Some(s);
        self
    }

    /// Adjust the server's 'addr'. This won't actually re-bind the server,
    /// it will just affect what the proxy think is the so_original_dst.
    ///
    /// This address is bogus, but the proxy should properly ignored the IP
    /// and only use the port combined with 127.0.0.1 to still connect to
    /// the server.
    pub fn inbound_fuzz_addr(self, mut s: server::Listening) -> Self {
        let old_addr = s.addr;
        let new_addr = ([10, 1, 2, 3], old_addr.port()).into();
        s.addr = new_addr;
        self.inbound(s)
    }

    pub fn outbound(mut self, s: server::Listening) -> Self {
        let addr = s.addr.clone();
        self.outbound = Some(addr);
        self.outbound_server = Some(s);
        self
    }

    pub fn outbound_ip(mut self, s: SocketAddr) -> Self {
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
        self.run_with_test_env(app::config::TestEnv::new())
    }

    pub fn run_with_test_env(self, env: app::config::TestEnv) -> Listening {
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

impl linkerd2_proxy::transport::GetOriginalDst for MockOriginalDst {
    fn get_original_dst(&self, sock: &transport::AddrInfo) -> Option<SocketAddr> {
        sock.local_addr().ok().and_then(|local| {
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

fn run(proxy: Proxy, mut env: app::config::TestEnv) -> Listening {
    let controller = proxy.controller.unwrap_or_else(|| controller::new().run());
    let inbound = proxy.inbound;
    let outbound = proxy.outbound;
    let identity = proxy.identity;
    let mut mock_orig_dst = DstInner::default();

    env.put(
        app::config::ENV_DESTINATION_SVC_ADDR,
        format!("{}", controller.addr),
    );
    env.put(
        app::config::ENV_OUTBOUND_LISTEN_ADDR,
        "127.0.0.1:0".to_owned(),
    );

    if let Some(inbound) = inbound {
        env.put(app::config::ENV_INBOUND_FORWARD, format!("{}", inbound));
        mock_orig_dst.inbound_orig_addr = Some(inbound);
    }

    mock_orig_dst.outbound_orig_addr = outbound;

    env.put(
        app::config::ENV_INBOUND_LISTEN_ADDR,
        "127.0.0.1:0".to_owned(),
    );
    env.put(
        app::config::ENV_CONTROL_LISTEN_ADDR,
        "127.0.0.1:0".to_owned(),
    );
    env.put(app::config::ENV_ADMIN_LISTEN_ADDR, "127.0.0.1:0".to_owned());

    static IDENTITY_SVC_NAME: &'static str = "LINKERD2_PROXY_IDENTITY_SVC_NAME";
    static IDENTITY_SVC_ADDR: &'static str = "LINKERD2_PROXY_IDENTITY_SVC_ADDR";

    let identity_addr = if let Some(ref identity) = identity {
        env.put(IDENTITY_SVC_NAME, "test-identity".to_owned());
        env.put(IDENTITY_SVC_ADDR, format!("{}", identity.addr));
        Some(identity.addr)
    } else {
        env.put(app::config::ENV_IDENTITY_DISABLED, "test".to_owned());
        None
    };

    if let Some(ports) = proxy.inbound_disable_ports_protocol_detection {
        let ports = ports
            .into_iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(",");
        env.put(
            app::config::ENV_INBOUND_PORTS_DISABLE_PROTOCOL_DETECTION,
            ports,
        );
    }

    if let Some(ports) = proxy.outbound_disable_ports_protocol_detection {
        let ports = ports
            .into_iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(",");
        env.put(
            app::config::ENV_OUTBOUND_PORTS_DISABLE_PROTOCOL_DETECTION,
            ports,
        );
    }

    let config = app::config::Config::parse(&env).unwrap();

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
            let _i = identity;

            let mock_orig_dst = MockOriginalDst(Arc::new(Mutex::new(mock_orig_dst)));
            // TODO: a mock timer could be injected here?
            let runtime =
                tokio::runtime::current_thread::Runtime::new().expect("initialize main runtime");
            // TODO: it would be nice for this to not be stubbed out, so that it
            // can be tested.
            let trace_handle = super::trace::LevelHandle::dangling();
            let main = linkerd2_proxy::app::Main::new(
                config,
                trace_handle,
                mock_orig_dst.clone(),
                runtime,
            );

            let control_addr = main.control_addr();
            let identity_addr = identity_addr;
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
                identity_addr,
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

    let (control_addr, identity_addr, inbound_addr, outbound_addr, metrics_addr) =
        running_rx.wait().unwrap();

    // printlns will show if the test fails...
    println!(
        "proxy running; destination={}, identity={:?}, inbound={}{}, outbound={}{}, metrics={}",
        control_addr
            .as_ref()
            .map(SocketAddr::to_string)
            .unwrap_or_else(String::new),
        identity_addr,
        inbound_addr,
        inbound
            .as_ref()
            .map(|i| format!(" (SO_ORIGINAL_DST={})", i))
            .unwrap_or_else(String::new),
        outbound_addr,
        outbound
            .as_ref()
            .map(|o| format!(" (SO_ORIGINAL_DST={})", o))
            .unwrap_or_else(String::new),
        metrics_addr,
    );

    Listening {
        control: control_addr,
        inbound: inbound_addr,
        outbound: outbound_addr,
        metrics: metrics_addr,

        outbound_server: proxy.outbound_server,
        inbound_server: proxy.inbound_server,

        shutdown: tx,
    }
}
