use crate::core::{
    admin, classify,
    config::ServerConfig,
    detect, drain, errors,
    metrics::{self, FmtMetrics},
    serve, tls, trace,
    transport::listen,
    Addr, Error,
};
use crate::{http, identity::LocalCrtKey, inbound, svc};
use std::{fmt, net::SocketAddr, pin::Pin, time::Duration};
use tokio::sync::mpsc;

#[derive(Clone, Debug)]
pub struct Config {
    pub server: ServerConfig,
    pub metrics_retain_idle: Duration,
}

pub struct Admin {
    pub listen_addr: SocketAddr,
    pub latch: admin::Latch,
    pub serve: Pin<Box<dyn std::future::Future<Output = Result<(), Error>> + Send + 'static>>,
}

#[derive(Debug, Default)]
pub struct AdminHttpOnly(());

impl Config {
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
    {
        const DETECT_TIMEOUT: Duration = Duration::from_secs(1);

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
            .push(svc::stack::MapTargetLayer::new(|(http_version, tcp)| {
                let inbound::TcpAccept {
                    target_addr,
                    client_id,
                    ..
                } = tcp;
                inbound::Target {
                    dst: Addr::Socket(target_addr),
                    target_addr,
                    http_version,
                    client_id,
                }
            }))
            .push(http::NewServeHttp::layer(Default::default(), drain.clone()))
            .push(svc::NewUnwrapOr::layer(
                svc::Fail::<_, AdminHttpOnly>::default(),
            ))
            .push(detect::NewDetectService::timeout(
                DETECT_TIMEOUT,
                http::DetectHttp::default(),
            ))
            .push_map_target(inbound::TcpAccept::from)
            .check_new_clone::<tls::server::Meta<listen::Addrs>>()
            .push(tls::NewDetectTls::layer(identity, DETECT_TIMEOUT))
            .into_inner();

        let serve = Box::pin(serve::serve(listen, admin, drain.signaled()));
        Ok(Admin {
            listen_addr,
            latch,
            serve,
        })
    }
}

impl fmt::Display for AdminHttpOnly {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("proxy admin server is HTTP-only")
    }
}

impl std::error::Error for AdminHttpOnly {}
