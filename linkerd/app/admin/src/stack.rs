use linkerd_app_core::{
    classify,
    config::ServerConfig,
    detect, drain, errors,
    metrics::{self, FmtMetrics},
    proxy::{http, identity::LocalCrtKey},
    serve,
    svc::{self, ExtractParam, InsertParam, Param},
    tls, trace,
    transport::{self, listen::Bind, ClientAddr, Local, Remote, ServerAddr},
    Error,
};
use linkerd_app_inbound as inbound;
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

#[derive(Clone, Debug)]
struct Tcp {
    addr: Local<ServerAddr>,
    client: Remote<ClientAddr>,
    tls: tls::ConditionalServerTls,
}

#[derive(Clone, Debug)]
struct Http {
    tcp: Tcp,
    version: http::Version,
}

#[derive(Clone)]
struct TlsParams {
    identity: Option<LocalCrtKey>,
}

const DETECT_TIMEOUT: Duration = Duration::from_secs(1);

// === impl Config ===

impl Config {
    #[allow(clippy::too_many_arguments)]
    pub fn build<B, R>(
        self,
        bind: B,
        identity: Option<LocalCrtKey>,
        report: R,
        metrics: inbound::Metrics,
        trace: trace::Handle,
        drain: drain::Watch,
        shutdown: mpsc::UnboundedSender<()>,
    ) -> Result<Task, Error>
    where
        R: FmtMetrics + Clone + Send + Sync + Unpin + 'static,
        B: Bind<ServerConfig>,
        B::Addrs: svc::Param<Remote<ClientAddr>> + svc::Param<Local<ServerAddr>>,
    {
        let (listen_addr, listen) = bind.bind(&self.server)?;

        let (ready, latch) = crate::server::Readiness::new();
        let admin = crate::server::Admin::new(report, ready, shutdown, trace);
        let admin = svc::stack(move |_| admin.clone())
            .push(metrics.proxy.http_endpoint.to_layer::<classify::Response, _, Http>())
            .push_on_response(
                svc::layers()
                    .push(metrics.http_errors.to_layer())
                    .push(errors::respond::layer())
                    .push(http::BoxResponse::layer()),
            )
            .push(http::NewServeHttp::layer(Default::default(), drain.clone()))
            .push_request_filter(
                |(http, tcp): (
                    Result<Option<http::Version>, detect::DetectTimeoutError<_>>,
                    Tcp,
                )| {
                    match http {
                        Ok(Some(version)) => Ok(Http { version, tcp }),
                        // If detection timed out, we can make an educated guess at the proper
                        // behavior:
                        // - If the connection was meshed, it was most likely transported over
                        //   HTTP/2.
                        // - If the connection was unmeshed, it was mostly likely HTTP/1.
                        // - If we received some unexpected SNI, the client is mostly likely
                        //   confused/stale.
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
                                    return Err(Error::from(UnexpectedSni(sni, tcp.client)));
                                }
                            };
                            debug!(%version, "HTTP detection timed out; assuming HTTP");
                            Ok(Http { version, tcp })
                        }
                        // If the connection failed HTTP detection, check if we detected TLS for
                        // another target. This might indicate that the client is confused/stale.
                        Ok(None) => match tcp.tls {
                            tls::ConditionalServerTls::Some(tls::ServerTls::Passthru { sni }) => {
                                Err(UnexpectedSni(sni, tcp.client).into())
                            }
                            _ => Err(NonHttpClient(tcp.client).into()),
                        },
                    }
                },
            )
            .push(svc::BoxNewService::layer())
            .push(detect::NewDetectService::layer(detect::Config::<http::DetectHttp>::from_timeout(DETECT_TIMEOUT)))
            .push(transport::metrics::NewServer::layer(metrics.proxy.transport))
            .push_map_target(|(tls, addrs): (tls::ConditionalServerTls, B::Addrs)| {
                Tcp {
                    tls,
                    client: addrs.param(),
                    addr: addrs.param(),
                }
            })
            .push(svc::BoxNewService::layer())
            .push(tls::NewDetectTls::layer(TlsParams {
                identity,
            }))
            .into_inner();

        let serve = Box::pin(serve::serve(listen, admin, drain.signaled()));
        Ok(Task {
            listen_addr,
            latch,
            serve,
        })
    }
}

// === impl Tcp ===

impl Param<transport::labels::Key> for Tcp {
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::inbound_server(
            self.tls.clone(),
            self.addr.into(),
            // TODO(ver) enforce policies on the proxy's admin port.
            Default::default(),
            Default::default(),
        )
    }
}

// === impl Http ===

impl Param<http::Version> for Http {
    fn param(&self) -> http::Version {
        self.version
    }
}

impl Param<metrics::EndpointLabels> for Http {
    fn param(&self) -> metrics::EndpointLabels {
        metrics::InboundEndpointLabels {
            tls: self.tcp.tls.clone(),
            authority: None,
            target_addr: self.tcp.addr.into(),
            policy: Default::default(),
        }
        .into()
    }
}

// === TlsParams ===

impl<T> ExtractParam<tls::server::Timeout, T> for TlsParams {
    #[inline]
    fn extract_param(&self, _: &T) -> tls::server::Timeout {
        tls::server::Timeout(DETECT_TIMEOUT)
    }
}

impl<T> ExtractParam<Option<LocalCrtKey>, T> for TlsParams {
    #[inline]
    fn extract_param(&self, _: &T) -> Option<LocalCrtKey> {
        self.identity.clone()
    }
}

impl<T> InsertParam<tls::ConditionalServerTls, T> for TlsParams {
    type Target = (tls::ConditionalServerTls, T);

    #[inline]
    fn insert_param(&self, tls: tls::ConditionalServerTls, target: T) -> Self::Target {
        (tls, target)
    }
}
