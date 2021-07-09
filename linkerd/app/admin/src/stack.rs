use linkerd_app_core::{
    classify,
    config::ServerConfig,
    detect, drain, errors,
    metrics::{self, FmtMetrics},
    proxy::{http, identity::LocalCrtKey},
    serve,
    svc::{self, Param},
    tls, trace,
    transport::{listen::Bind, ClientAddr, Local, Remote, ServerAddr},
    Error,
};
use linkerd_app_inbound::target::{HttpAccept, Target, TcpAccept};
use std::{pin::Pin, time::Duration};
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::debug;

#[derive(Clone, Debug)]
pub struct Config {
    pub server: ServerConfig,
    pub metrics_retain_idle: Duration,
}

pub struct Task {
    pub listen_addr: Local<ServerAddr>,
    pub latch: crate::Latch,
    pub serve: Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
}

#[derive(Debug, Error)]
#[error("non-HTTP connection from {}", self.0)]
struct NonHttpClient(Remote<ClientAddr>);

#[derive(Debug, Error)]
#[error("Unexpected TLS connection to {} from {}", self.0, self.1)]
struct UnexpectedSni(tls::ServerId, Remote<ClientAddr>);

// === impl Config ===

impl Config {
    #[allow(clippy::too_many_arguments)]
    pub fn build<B, R>(
        self,
        bind: B,
        identity: Option<LocalCrtKey>,
        report: R,
        metrics: metrics::Proxy,
        trace: trace::Handle,
        drain: drain::Watch,
        shutdown: mpsc::UnboundedSender<()>,
    ) -> Result<Task, Error>
    where
        R: FmtMetrics + Clone + Send + Sync + Unpin + 'static,
        B: Bind<ServerConfig>,
        B::Addrs: svc::Param<Remote<ClientAddr>> + svc::Param<Local<ServerAddr>>,
    {
        const DETECT_TIMEOUT: Duration = Duration::from_secs(1);

        let (listen_addr, listen) = bind.bind(&self.server)?;

        let (ready, latch) = crate::server::Readiness::new();
        let admin = crate::server::Admin::new(report, ready, shutdown, trace);
        let admin = svc::stack(admin)
            .push(metrics.http_endpoint.to_layer::<classify::Response, _, Target>())
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
                        // If detection timed out, we can make an educated guess
                        // at the proper behavior:
                        // - If the connection was meshed, it was most likely
                        //   transported over HTTP/2.
                        // - If the connection was unmehsed, it was mostly
                        //   likely HTTP/1.
                        // - If we received some unexpected SNI, the client is
                        //   mostly likely confused/stale.
                        Err(_timeout) => {
                            let version = match tcp.tls.clone() {
                                tls::ConditionalServerTls::None(_) => http::Version::Http1,
                                tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                                    ..
                                }) => http::Version::H2,
                                tls::ConditionalServerTls::Some(tls::ServerTls::Passthru {
                                    sni,
                                }) => {
                                    debug_assert!(false, "If we know the stream is non-mesh TLS, we should be able to prove its not HTTP.");
                                    return Err(Error::from(UnexpectedSni(sni, tcp.client_addr)));
                                }
                            };
                            debug!(%version, "HTTP detection timed out; assuming HTTP");
                            Ok(HttpAccept::from((version, tcp)))
                        }
                        // If the connection failed HTTP detection, check if we
                        // detected TLS for another target. This might indicate
                        // that the client is confused/stale.
                        Ok(None) => match tcp.tls {
                            tls::ConditionalServerTls::Some(tls::ServerTls::Passthru { sni }) => {
                                Err(UnexpectedSni(sni, tcp.client_addr).into())
                            }
                            _ => Err(NonHttpClient(tcp.client_addr).into()),
                        },
                    }
                },
            )
            .push(svc::BoxNewService::layer())
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
            .push(svc::BoxNewService::layer())
            .push(tls::NewDetectTls::layer(identity, DETECT_TIMEOUT))
            .into_inner();

        let serve = Box::pin(serve::serve(listen, admin, drain.signaled()));
        Ok(Task {
            listen_addr,
            latch,
            serve,
        })
    }
}
