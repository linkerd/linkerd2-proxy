use crate::core::{
    admin, classify,
    config::ServerConfig,
    detect, drain, errors,
    metrics::{self, FmtMetrics},
    serve, tls, trace,
    transport::listen,
    Error,
};
use crate::{
    http,
    identity::LocalCrtKey,
    inbound::target::{HttpAccept, Target, TcpAccept},
    svc,
};
use std::{net::SocketAddr, pin::Pin, time::Duration};
use tokio::sync::mpsc;
use tracing::debug;

#[derive(Clone, Debug)]
pub struct Config {
    pub server: ServerConfig,
    pub metrics_retain_idle: Duration,
}

pub struct Admin {
    pub listen_addr: SocketAddr,
    pub latch: admin::Latch,
    pub serve: Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
}

// === impl Config ===

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
            .push_map_target(Target::from)
            .push(http::NewServeHttp::layer(Default::default(), drain.clone()))
            .push_map_target(
                |(version, tcp): (Result<Option<http::Version>, detect::DetectTimeout<_>>, _)| {
                    match version {
                        Ok(Some(version)) => HttpAccept::from((version, tcp)),
                        Ok(None) | Err(_) => {
                            debug!("Failed to parse HTTP request; handling as HTTP/1");
                            HttpAccept::from((http::Version::Http1, tcp))
                        }
                    }
                },
            )
            .push(detect::NewDetectService::layer(
                DETECT_TIMEOUT,
                http::DetectHttp::default(),
            ))
            .push(metrics.transport.layer_accept())
            .push_map_target(TcpAccept::from_local_addr)
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
