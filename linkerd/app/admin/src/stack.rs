use linkerd_app_core::{
    classify,
    config::ServerConfig,
    detect, drain, errors, identity,
    metrics::{self, FmtMetrics},
    proxy::http,
    serve,
    svc::{self, ExtractParam, InsertParam, Param},
    tls, trace,
    transport::{
        self, addrs::AddrPair, listen::Bind, ClientAddr, Local, OrigDstAddr, Remote, ServerAddr,
    },
    Error, Result,
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
    #[cfg(feature = "pprof")]
    pub enable_profiling: bool,
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
struct UnexpectedSni(tls::ServerName, Remote<ClientAddr>);

#[derive(Clone, Debug)]
struct Tcp {
    policy: inbound::policy::AllowPolicy,
    addr: Local<ServerAddr>,
    client: Remote<ClientAddr>,
    tls: tls::ConditionalServerTls,
}

#[derive(Clone, Debug)]
struct Http {
    tcp: Tcp,
    version: http::Version,
}

#[derive(Clone, Debug)]
struct Permitted {
    permit: inbound::policy::HttpRoutePermit,
    http: Http,
}

#[derive(Clone)]
struct TlsParams {
    identity: identity::Server,
}

const DETECT_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Copy, Clone, Debug)]
struct Rescue;

// === impl Config ===

impl Config {
    #[allow(clippy::too_many_arguments)]
    pub fn build<B, R>(
        self,
        bind: B,
        policy: impl inbound::policy::GetPolicy,
        identity: identity::Server,
        report: R,
        metrics: inbound::InboundMetrics,
        trace: trace::Handle,
        drain: drain::Watch,
        shutdown: mpsc::UnboundedSender<()>,
    ) -> Result<Task>
    where
        R: FmtMetrics + Clone + Send + Sync + Unpin + 'static,
        B: Bind<ServerConfig>,
        B::Addrs: svc::Param<Remote<ClientAddr>>,
        B::Addrs: svc::Param<Local<ServerAddr>>,
        B::Addrs: svc::Param<AddrPair>,
    {
        let (listen_addr, listen) = bind.bind(&self.server)?;

        // Get the policy for the admin server.
        let policy = policy.get_policy(OrigDstAddr(listen_addr.into()));

        let (ready, latch) = crate::server::Readiness::new();

        #[cfg_attr(not(feature = "pprof"), allow(unused_mut))]
        let admin = crate::server::Admin::new(report, ready, shutdown, trace);

        #[cfg(feature = "pprof")]
        let admin = admin.with_profiling(self.enable_profiling);

        let http = svc::stack(move |_| admin.clone())
            .push(
                metrics
                    .proxy
                    .http_endpoint
                    .to_layer::<classify::Response, _, Permitted>(),
            )
            .push(classify::NewClassify::layer_default())
            .push_map_target(|(permit, http)| Permitted { permit, http })
            .push(inbound::policy::NewHttpPolicy::layer(
                metrics.http_authz.clone(),
            ))
            .push(Rescue::layer())
            .push_on_service(http::BoxResponse::layer())
            .arc_new_clone_http();

        let tcp = http
            .unlift_new()
            .push(http::NewServeHttp::layer({
                let drain = drain.clone();
                move |t: &Http| {
                    http::ServerParams {
                        version: t.version,
                        h2: Default::default(),
                        drain: drain.clone(),
                    }
                }
            }))
            .push_filter(
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
                            let version = match tcp.tls {
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
                            debug!(?version, "HTTP detection timed out; assuming HTTP");
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
            .arc_new_tcp()
            .lift_new_with_target()
            .push(detect::NewDetectService::layer(svc::stack::CloneParam::from(
                detect::Config::<http::DetectHttp>::from_timeout(DETECT_TIMEOUT),
            )))
            .push(transport::metrics::NewServer::layer(metrics.proxy.transport))
            .push_map_target(move |(tls, addrs): (tls::ConditionalServerTls, B::Addrs)| {
                Tcp {
                    tls,
                    client: addrs.param(),
                    addr: addrs.param(),
                    policy: policy.clone(),
                }
            })
            .arc_new_tcp()
            .push(tls::NewDetectTls::<identity::Server, _, _>::layer(TlsParams {
                identity,
            }))
            .arc_new_tcp()
            .into_inner();

        let serve = Box::pin(serve::serve(listen, tcp, drain.signaled()));
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
            self.policy.server_label(),
        )
    }
}

// === impl Http ===

impl Param<http::Version> for Http {
    fn param(&self) -> http::Version {
        self.version
    }
}

impl Param<OrigDstAddr> for Http {
    fn param(&self) -> OrigDstAddr {
        OrigDstAddr(self.tcp.addr.into())
    }
}

impl Param<Remote<ClientAddr>> for Http {
    fn param(&self) -> Remote<ClientAddr> {
        self.tcp.client
    }
}

impl Param<AddrPair> for Http {
    fn param(&self) -> AddrPair {
        let Remote(client) = self.tcp.client;
        let Local(server) = self.tcp.addr;
        AddrPair(client, server)
    }
}

impl Param<tls::ConditionalServerTls> for Http {
    fn param(&self) -> tls::ConditionalServerTls {
        self.tcp.tls.clone()
    }
}

impl Param<inbound::policy::AllowPolicy> for Http {
    fn param(&self) -> inbound::policy::AllowPolicy {
        self.tcp.policy.clone()
    }
}

impl Param<metrics::ServerLabel> for Http {
    fn param(&self) -> metrics::ServerLabel {
        self.tcp.policy.server_label()
    }
}

// === impl Permitted ===

impl Param<metrics::EndpointLabels> for Permitted {
    fn param(&self) -> metrics::EndpointLabels {
        metrics::InboundEndpointLabels {
            tls: self.http.tcp.tls.clone(),
            authority: None,
            target_addr: self.http.tcp.addr.into(),
            policy: self.permit.labels.clone(),
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

impl<T> ExtractParam<identity::Server, T> for TlsParams {
    #[inline]
    fn extract_param(&self, _: &T) -> identity::Server {
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

// === impl Rescue ===

impl Rescue {
    /// Synthesizes responses for HTTP requests that encounter errors.
    fn layer<N>(
    ) -> impl svc::layer::Layer<N, Service = errors::NewRespondService<Self, Self, N>> + Clone {
        errors::respond::layer(Self)
    }
}

impl<T> ExtractParam<Self, T> for Rescue {
    #[inline]
    fn extract_param(&self, _: &T) -> Self {
        Self
    }
}

impl<T: Param<tls::ConditionalServerTls>> ExtractParam<errors::respond::EmitHeaders, T> for Rescue {
    #[inline]
    fn extract_param(&self, t: &T) -> errors::respond::EmitHeaders {
        // Only emit informational headers to meshed peers.
        let emit = t
            .param()
            .value()
            .map(|tls| match tls {
                tls::ServerTls::Established { client_id, .. } => client_id.is_some(),
                _ => false,
            })
            .unwrap_or(false);
        errors::respond::EmitHeaders(emit)
    }
}

impl errors::HttpRescue<Error> for Rescue {
    fn rescue(&self, error: Error) -> Result<errors::SyntheticHttpResponse> {
        if let Some(cause) = errors::cause_ref::<inbound::policy::HttpRouteNotFound>(&*error) {
            return Ok(errors::SyntheticHttpResponse::not_found(cause));
        }

        if let Some(cause) = errors::cause_ref::<inbound::policy::HttpRouteUnauthorized>(&*error) {
            return Ok(errors::SyntheticHttpResponse::permission_denied(cause));
        }

        tracing::warn!(error, "Unexpected error");
        Ok(errors::SyntheticHttpResponse::unexpected_error())
    }
}
