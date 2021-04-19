use crate::core::{
    admin, classify,
    config::ServerConfig,
    detect, drain, errors,
    metrics::{self, FmtMetrics},
    serve,
    svc::{self, Param},
    tls, trace,
    transport::{listen::Bind, ClientAddr, Local, Remote, ServerAddr},
    Error,
};
use crate::{
    http,
    identity::LocalCrtKey,
    inbound::target::{HttpAccept, Target, TcpAccept},
};
use std::{pin::Pin, time::Duration};
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::debug;

#[derive(Clone, Debug)]
pub struct Config {
    pub server: ServerConfig,
    pub metrics_retain_idle: Duration,
}

pub struct Admin {
    pub listen_addr: Local<ServerAddr>,
    pub latch: admin::Latch,
    pub serve: Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
}

#[derive(Debug, Error)]
#[error("non-HTTP connection from {} (tls={:?})", self.0.client_addr, self.0.tls)]
struct NonHttpClient(TcpAccept);

// === impl Config ===

impl Config {
    #[allow(clippy::clippy::too_many_arguments)]
    pub fn build<B, R>(
        self,
        bind: B,
        identity: Option<LocalCrtKey>,
        report: R,
        metrics: metrics::Proxy,
        trace: trace::Handle,
        drain: drain::Watch,
        shutdown: mpsc::UnboundedSender<()>,
    ) -> Result<Admin, Error>
    where
        R: FmtMetrics + Clone + Send + 'static + Unpin,
        B: Bind<ServerConfig>,
        B::Addrs: svc::Param<Remote<ClientAddr>> + svc::Param<Local<ServerAddr>>,
    {
        const DETECT_TIMEOUT: Duration = Duration::from_secs(1);

        let (listen_addr, listen) = bind.bind(&self.server)?;

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
            .push_request_filter(
                |(version, tcp): (
                    Result<Option<http::Version>, detect::DetectTimeout<_>>,
                    TcpAccept,
                )| {
                    match version {
                        Ok(Some(version)) => Ok(HttpAccept::from((version, tcp))),
                        Err(_) => {
                            debug!("HTTP detection timed out; handling as HTTP/1");
                            Ok(HttpAccept::from((http::Version::Http1, tcp)))
                        }
                        Ok(None) => Err(NonHttpClient(tcp)),
                    }
                },
            )
            .push(detect::NewDetectService::layer(
                DETECT_TIMEOUT,
                http::DetectHttp::default(),
            ))
            .push(metrics.transport.layer_accept())
            .push_map_target(|(tls, addrs): (tls::ConditionalServerTls, B::Addrs)| {
                // TODO this should use an admin-specific target type.
                let Local(ServerAddr(target_addr)) = addrs.param();
                TcpAccept {
                    tls,
                    client_addr: addrs.param(),
                    target_addr,
                }
            })
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
