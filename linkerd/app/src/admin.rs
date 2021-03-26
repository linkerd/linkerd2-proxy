use crate::core::{
    admin, classify,
    config::ServerConfig,
    detect, drain, errors, io,
    metrics::{self, FmtMetrics},
    serve, tls, trace,
    transport::{self, listen::GetAddrs, ClientAddr, Local, Remote, ServerAddr},
    Error,
};
use crate::{
    http,
    identity::LocalCrtKey,
    inbound::target::{HttpAccept, Target, TcpAccept},
    svc::{self, Param},
};
use std::{fmt, net::SocketAddr, pin::Pin, time::Duration};
use tokio::{net::TcpStream, sync::mpsc};

#[derive(Clone, Debug)]
pub struct Config<A> {
    pub server: ServerConfig<A>,
    pub metrics_retain_idle: Duration,
}

pub struct Admin {
    pub listen_addr: SocketAddr,
    pub latch: admin::Latch,
    pub serve: Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
}

#[derive(Debug, Clone, Copy)]
pub struct Addrs {
    server: Local<ServerAddr>,
    client: Remote<ClientAddr>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct GetAdminAddrs(());

#[derive(Debug, Default)]
pub struct AdminHttpOnly(());

// === impl Config ===

impl<A> Config<A> {
    pub fn build<R>(
        self,
        identity: Option<LocalCrtKey>,
        report: R,
        metrics: metrics::Proxy,
        trace: trace::Handle,
        drain: drain::Watch,
        shutdown: mpsc::UnboundedSender<()>,
    ) -> Result<Admin, Error>
    where
        R: FmtMetrics + Clone + Send + 'static + Unpin,
        A: GetAddrs<io::ScopedIo<TcpStream>> + Clone + Send + Sync + 'static,
        A::Addrs:
            Param<Remote<ClientAddr>> + Param<Local<ServerAddr>> + Clone + Send + Sync + 'static,
    {
        const DETECT_TIMEOUT: Duration = Duration::from_secs(1);

        let get_addrs = self.server.orig_dst_addrs;
        let (listen_addr, listen) = self.server.bind.bind()?;

        let (ready, latch) = admin::Readiness::new();
        let admin = admin::Admin::new(report, ready, shutdown, trace);
        let admin = svc::stack(admin)
            .push(metrics.http_endpoint.to_layer::<classify::Response, _>())
            .push_on_response(
                svc::layers()
                    .push(metrics.http_errors.clone())
                    .push(errors::layer())
                    .push(http::BoxResponse::layer()),
            )
            .push_map_target(Target::from)
            .push(http::NewServeHttp::layer(Default::default(), drain.clone()))
            .push(svc::Filter::<TcpAccept, _>::layer(
                |(version, tcp): (Result<Option<http::Version>, detect::DetectTimeout<_>>, _)| {
                    match version {
                        Ok(Some(version)) => Ok(HttpAccept::from((version, tcp))),
                        Ok(None) => Err(Error::from(AdminHttpOnly(()))),
                        Err(timeout) => Err(Error::from(timeout)),
                    }
                },
            ))
            .push(detect::NewDetectService::layer(
                DETECT_TIMEOUT,
                http::DetectHttp::default(),
            ))
            .push(metrics.transport.layer_accept())
            .push_map_target(TcpAccept::from_local_addr)
            .check_new_clone::<tls::server::Meta<A::Addrs>>()
            .push(tls::NewDetectTls::layer(identity, DETECT_TIMEOUT))
            .instrument(|a: &A::Addrs| {
                let Remote(ClientAddr(addr)) = a.param();
                tracing::debug_span!("accept", server = %"admin", client.addr = %addr)
            })
            .push_request_filter(transport::AddrsFilter(get_addrs))
            .into_inner();

        let serve = Box::pin(serve::serve(listen, admin, drain.signaled()));
        Ok(Admin {
            listen_addr,
            latch,
            serve,
        })
    }
}

// === impl AdminHttpOnly ===

impl fmt::Display for AdminHttpOnly {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("proxy admin server is HTTP-only")
    }
}

impl std::error::Error for AdminHttpOnly {}

// === impl AdminAddrs ===

impl svc::Param<Local<ServerAddr>> for Addrs {
    fn param(&self) -> Local<ServerAddr> {
        self.server
    }
}

impl svc::Param<Remote<ClientAddr>> for Addrs {
    fn param(&self) -> Remote<ClientAddr> {
        self.client
    }
}

// === impl GetAdminAddrs ===

impl GetAdminAddrs {
    pub fn new() -> Self {
        Self::default()
    }
}

impl<T> GetAddrs<T> for GetAdminAddrs
where
    T: AsRef<TcpStream>,
{
    type Addrs = Addrs;
    fn addrs(&self, tcp: &T) -> io::Result<Self::Addrs> {
        let tcp = tcp.as_ref();
        let server = Local(ServerAddr(tcp.local_addr()?));
        let client = Remote(ClientAddr(tcp.peer_addr()?));
        tracing::trace!(
            server.addr = %server,
            client.addr = %client,
            "Accepted",
        );
        Ok(Addrs { server, client })
    }
}
