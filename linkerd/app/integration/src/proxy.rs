use super::*;
use app_core::transport::OrigDstAddr;
use linkerd_app_core::{
    svc::Param,
    transport::{listen, orig_dst, Keepalive, ListenAddr},
};
use std::{future::Future, net::SocketAddr, pin::Pin, task::Poll, thread};
use tokio::net::TcpStream;
use tracing::instrument::Instrument;

pub fn new() -> Proxy {
    Proxy::default()
}

#[derive(Default)]
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

    shutdown_signal: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
}

#[derive(Copy, Clone, Debug)]
struct MockOrigDst(Option<SocketAddr>);

pub struct Listening {
    pub tap: Option<SocketAddr>,
    pub inbound: SocketAddr,
    pub outbound: SocketAddr,
    pub metrics: SocketAddr,

    pub outbound_server: Option<server::Listening>,
    pub inbound_server: Option<server::Listening>,

    controller: controller::Listening,
    identity: Option<controller::Listening>,

    shutdown: Shutdown,
    terminated: oneshot::Receiver<()>,

    thread: thread::JoinHandle<()>,
}

// === impl MockOrigDst ===

impl<T> listen::Bind<T> for MockOrigDst
where
    T: Param<Keepalive> + Param<ListenAddr>,
{
    type Addrs = orig_dst::Addrs;
    type Io = tokio::net::TcpStream;
    type Incoming = Pin<
        Box<dyn Stream<Item = io::Result<(orig_dst::Addrs, TcpStream)>> + Send + Sync + 'static>,
    >;

    fn bind(self, params: &T) -> io::Result<listen::Bound<Self::Incoming>> {
        let (bound, incoming) = listen::BindTcp::default().bind(params)?;
        let addr = self.0;
        let incoming = Box::pin(incoming.map(move |res| {
            let (inner, tcp) = res?;
            let orig_dst = addr
                .map(OrigDstAddr)
                .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "No mocked SO_ORIG_DST"))?;
            let addrs = orig_dst::Addrs { inner, orig_dst };
            Ok((addrs, tcp))
        }));
        Ok((bound, incoming))
    }
}

// === impl Proxy ===

impl Proxy {
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
        self.inbound = Some(s.addr);
        self.inbound_server = Some(s);
        self
    }

    pub fn inbound_ip(mut self, s: SocketAddr) -> Self {
        self.inbound = Some(s);
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
        self.outbound = Some(s.addr);
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

    pub async fn run(self) -> Listening {
        self.run_with_test_env(TestEnv::default()).await
    }

    pub async fn run_with_test_env(self, env: TestEnv) -> Listening {
        run(self, env, true).await
    }

    pub async fn run_with_test_env_and_keep_ports(self, env: TestEnv) -> Listening {
        run(self, env, false).await
    }
}

impl Listening {
    pub async fn join_servers(self) {
        let Self {
            inbound_server,
            outbound_server,
            controller,
            identity,
            thread,
            shutdown,
            terminated,
            ..
        } = self;

        debug!("signaling shutdown");
        shutdown.signal();

        debug!("waiting for proxy termination");
        terminated.await.unwrap();

        debug!("proxy terminated");
        thread.join().unwrap();

        let outbound = async move {
            if let Some(srv) = outbound_server {
                srv.join().await;
            }
        }
        .instrument(tracing::info_span!("outbound"));

        let inbound = async move {
            if let Some(srv) = inbound_server {
                srv.join().await;
            }
        }
        .instrument(tracing::info_span!("inbound"));

        let identity = async move {
            if let Some(srv) = identity {
                srv.join().await;
            }
        };

        tokio::join! {
            inbound,
            outbound,
            identity,
            controller.join(),
        };
    }
}

async fn run(proxy: Proxy, mut env: TestEnv, random_ports: bool) -> Listening {
    use app::env::Strings;

    let controller = if let Some(controller) = proxy.controller {
        controller
    } else {
        // bummer that the whole function needs to be async just for this...
        controller::new().run().await
    };
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

    static IDENTITY_SVC_NAME: &str = "LINKERD2_PROXY_IDENTITY_SVC_NAME";
    static IDENTITY_SVC_ADDR: &str = "LINKERD2_PROXY_IDENTITY_SVC_ADDR";

    let identity_addr = if let Some(ref identity) = identity {
        env.put(IDENTITY_SVC_NAME, "test-identity".to_owned());
        env.put(IDENTITY_SVC_ADDR, format!("{}", identity.addr));
        Some(identity.addr)
    } else {
        env.put(app::env::ENV_IDENTITY_DISABLED, "test".to_owned());
        None
    };

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

    if !env.contains_key(app::env::ENV_DESTINATION_PROFILE_NETWORKS) {
        // If the test has not already overridden the destination search
        // networks, make sure that localhost works.
        //
        // If there's already a value, we can assume that the test cares about
        // this and may not want to do profile lookups on localhost.
        env.put(
            app::env::ENV_DESTINATION_PROFILE_NETWORKS,
            "127.0.0.0/24".to_owned(),
        );
    }

    let config = app::env::parse_config(&env).unwrap();

    let dispatch = tracing::Dispatch::default();
    let (trace, trace_handle) = if dispatch
        .downcast_ref::<tracing_subscriber::fmt::TestWriter>()
        .is_some()
    {
        // A dispatcher was set by the test...
        (dispatch, app_core::trace::Handle::disabled())
    } else {
        eprintln!("test did not set a tracing dispatcher, creating a new one for the proxy");
        super::trace_subscriber()
    };

    let (running_tx, running_rx) = oneshot::channel();
    let (term_tx, term_rx) = oneshot::channel();
    let (tx, mut rx) = shutdown_signal();

    if let Some(fut) = proxy.shutdown_signal {
        rx = Box::pin(async move {
            tokio::select! {
                _ = rx => {},
                _ = fut => {},
            }
        });
    }

    let thread = thread::Builder::new()
        .name(format!("{}:proxy", thread_name()))
        .spawn(move || {
            tracing::dispatcher::with_default(&trace, || {
                let span = info_span!("proxy", test = %thread_name());
                let _enter = span.enter();

                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("proxy")
                    .block_on(async move {
                        let bind_in = MockOrigDst(inbound);
                        let bind_out = MockOrigDst(outbound);
                        let bind_adm = listen::BindTcp::default();
                        let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::unbounded_channel();
                        let main = config
                            .build(bind_in, bind_out, bind_adm, shutdown_tx, trace_handle)
                            .await
                            .expect("config");

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

                        tokio::select! {
                            _ = on_shutdown => {
                                debug!("after on_shutdown");
                            }
                            _ = shutdown_rx.recv() => {}
                        }

                        drain.drain().await;
                        debug!("after drain");

                        // Suppress error as not all tests wait for graceful shutdown
                        let _ = term_tx.send(());
                    });
            })
        })
        .expect("spawn");

    let (tap_addr, identity_addr, inbound_addr, outbound_addr, metrics_addr) =
        running_rx.await.unwrap();

    tracing::info!(
        tap.addr = ?tap_addr,
        identity.addr = ?identity_addr,
        inbound.addr = ?inbound_addr,
        inbound.orig_dst = ?inbound.as_ref(),
        outbound.addr = ?outbound_addr,
        outbound.orig_dst = ?outbound.as_ref(),
        metrics.addr = ?metrics_addr,
    );

    Listening {
        tap: tap_addr.map(Into::into),
        inbound: inbound_addr.into(),
        outbound: outbound_addr.into(),
        metrics: metrics_addr.into(),

        outbound_server: proxy.outbound_server,
        inbound_server: proxy.inbound_server,

        controller,
        identity,

        shutdown: tx,
        terminated: term_rx,
        thread,
    }
}
