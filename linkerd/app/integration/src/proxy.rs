use super::*;
use std::future::Future;
use std::pin::Pin;
use std::task::Poll;

pub fn new() -> Proxy {
    Proxy::new()
}

pub struct Proxy {
    controller: Option<controller::Listening>,
    identity: Option<controller::Listening>,

    /// Inbound/outbound addresses helpful for mocking connections that do not
    /// implement `server::Listener`.
    inbound: Option<SocketAddr>,
    outbound: Option<SocketAddr>,

    /// Inbound/outbound addresses for mocking connections that implement
    /// `server::Listener`.
    inbound_server: Option<server::Listening>,
    outbound_server: Option<server::Listening>,

    inbound_disable_ports_protocol_detection: Option<Vec<u16>>,
    outbound_disable_ports_protocol_detection: Option<Vec<u16>>,

    shutdown_signal: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
}

pub struct Listening {
    pub tap: Option<SocketAddr>,
    pub inbound: SocketAddr,
    pub outbound: SocketAddr,
    pub metrics: SocketAddr,

    pub outbound_server: Option<server::Listening>,
    pub inbound_server: Option<server::Listening>,

    _shutdown: Shutdown,
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

    pub fn disable_identity(mut self) -> Self {
        self.identity = None;
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
        let fut = Box::pin(sig.map(|_| ()));
        self.shutdown_signal = Some(fut);
        self
    }

    pub fn run(self) -> Listening {
        self.run_with_test_env(TestEnv::new())
    }

    pub fn run_with_test_env(self, env: TestEnv) -> Listening {
        run(self, env, true)
    }

    pub fn run_with_test_env_and_keep_ports(self, env: TestEnv) -> Listening {
        run(self, env, false)
    }
}

fn run(proxy: Proxy, mut env: TestEnv, random_ports: bool) -> Listening {
    use app::env::Strings;

    let controller = proxy.controller.unwrap_or_else(|| controller::new().run());
    let inbound = proxy.inbound;
    let outbound = proxy.outbound;
    let identity = proxy.identity;

    env.put(
        "LINKERD2_PROXY_DESTINATION_SVC_ADDR",
        controller.addr.to_string(),
    );
    if random_ports {
        env.put(app::env::ENV_OUTBOUND_LISTEN_ADDR, "127.0.0.1:0".to_owned());
    }

    // If a given server is missing, use the admin server as a substitute.
    env.put(
        app::env::ENV_INBOUND_ORIG_DST_ADDR,
        inbound.unwrap_or(controller.addr).to_string(),
    );
    env.put(
        app::env::ENV_OUTBOUND_ORIG_DST_ADDR,
        outbound.unwrap_or(controller.addr).to_string(),
    );

    if random_ports {
        env.put(app::env::ENV_INBOUND_LISTEN_ADDR, "127.0.0.1:0".to_owned());
        env.put(app::env::ENV_CONTROL_LISTEN_ADDR, "127.0.0.1:0".to_owned());
        env.put(app::env::ENV_ADMIN_LISTEN_ADDR, "127.0.0.1:0".to_owned());
    } else {
        let local_inbound = env
            .get(app::env::ENV_INBOUND_LISTEN_ADDR)
            .unwrap_or(None)
            .unwrap_or_else(|| app::env::DEFAULT_INBOUND_LISTEN_ADDR.to_owned());
        env.put(app::env::ENV_INBOUND_LISTEN_ADDR, local_inbound);
        let local_control = env
            .get(app::env::ENV_CONTROL_LISTEN_ADDR)
            .unwrap_or(None)
            .unwrap_or_else(|| app::env::DEFAULT_CONTROL_LISTEN_ADDR.to_owned());
        env.put(app::env::ENV_CONTROL_LISTEN_ADDR, local_control);
    }

    static IDENTITY_SVC_NAME: &'static str = "LINKERD2_PROXY_IDENTITY_SVC_NAME";
    static IDENTITY_SVC_ADDR: &'static str = "LINKERD2_PROXY_IDENTITY_SVC_ADDR";

    let identity_addr = if let Some(ref identity) = identity {
        env.put(IDENTITY_SVC_NAME, "test-identity".to_owned());
        env.put(IDENTITY_SVC_ADDR, format!("{}", identity.addr));
        Some(identity.addr)
    } else {
        env.put(app::env::ENV_IDENTITY_DISABLED, "test".to_owned());
        env.put(app::env::ENV_TAP_DISABLED, "test".to_owned());
        None
    };

    // If identity is enabled but the test is not concerned with tap, ensure
    // there is a tap service name set
    if !env.contains_key(app::env::ENV_TAP_DISABLED)
        && !env.contains_key(app::env::ENV_TAP_SVC_NAME)
    {
        env.put(app::env::ENV_TAP_SVC_NAME, "test-identity".to_owned())
    }

    if let Some(ports) = proxy.inbound_disable_ports_protocol_detection {
        let ports = ports
            .into_iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(",");
        env.put(
            app::env::ENV_INBOUND_PORTS_DISABLE_PROTOCOL_DETECTION,
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
            app::env::ENV_OUTBOUND_PORTS_DISABLE_PROTOCOL_DETECTION,
            ports,
        );
    }

    let config = app::env::parse_config(&env).unwrap();
    let (trace, trace_handle) = super::trace_init();

    let (running_tx, running_rx) = oneshot::channel();
    let (tx, mut rx) = shutdown_signal();

    if let Some(fut) = proxy.shutdown_signal {
        rx = Box::pin(async move {
            tokio::select! {
                _ = rx => {},
                _ = fut => {},
            }
        });
    }

    std::thread::Builder::new()
        .name(format!("{}:proxy", thread_name()))
        .spawn(move || {
            tracing::dispatcher::with_default(&trace, || {
                let span = info_span!("proxy", test = %thread_name());
                let _enter = span.enter();

                let _c = controller;
                let _i = identity;

                tokio_compat::runtime::current_thread::Runtime::new()
                    .expect("proxy")
                    .block_on_std(async move {
                        let main = config.build(trace_handle).expect("config");

                        // slip the running tx into the shutdown future, since the first time
                        // the shutdown future is polled, that means all of the proxy is now
                        // running.
                        let addrs = (
                            main.tap_addr(),
                            identity_addr,
                            main.inbound_addr(),
                            main.outbound_addr(),
                            main.admin_addr(),
                        );
                        let mut running = Some((running_tx, addrs));
                        let on_shutdown = futures::future::poll_fn::<(), _>(move |cx| {
                            if let Some((tx, addrs)) = running.take() {
                                let _ = tx.send(addrs);
                            }

                            futures::ready!((&mut rx).as_mut().poll(cx));
                            debug!("shutdown");
                            Poll::Ready(())
                        });

                        let drain = main.spawn();
                        on_shutdown.await;
                        drain.drain().await;
                    });
            })
        })
        .expect("spawn");

    let (tap_addr, identity_addr, inbound_addr, outbound_addr, metrics_addr) =
        futures::executor::block_on(running_rx).unwrap();

    // printlns will show if the test fails...
    println!(
        "proxy running; tap={}, identity={:?}, inbound={}{}, outbound={}{}, metrics={}",
        tap_addr
            .as_ref()
            .map(SocketAddr::to_string)
            .unwrap_or_default(),
        identity_addr,
        inbound_addr,
        inbound
            .as_ref()
            .map(|i| format!(" (SO_ORIGINAL_DST={})", i))
            .unwrap_or_default(),
        outbound_addr,
        outbound
            .as_ref()
            .map(|o| format!(" (SO_ORIGINAL_DST={})", o))
            .unwrap_or_default(),
        metrics_addr,
    );

    Listening {
        tap: tap_addr,
        inbound: inbound_addr,
        outbound: outbound_addr,
        metrics: metrics_addr,

        outbound_server: proxy.outbound_server,
        inbound_server: proxy.inbound_server,

        _shutdown: tx,
    }
}
