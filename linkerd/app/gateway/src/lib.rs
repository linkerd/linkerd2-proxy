#![deny(warnings, rust_2018_idioms)]

mod gateway;
#[cfg(test)]
mod tests;

use self::gateway::NewGateway;
use linkerd_app_core::{
    config::ProxyConfig,
    detect, discovery_rejected, drain, io, metrics,
    opencensus::proto::trace::v1 as oc,
    profiles,
    proxy::http,
    svc::{self, stack::Param},
    tls,
    transport_header::SessionProtocol,
    Error, NameAddr, NameMatch,
};
use linkerd_app_inbound::{
    self as inbound,
    direct::{ClientInfo, GatewayConnection},
};
use linkerd_app_outbound as outbound;
use std::convert::TryInto;
use tokio::sync::mpsc;
use tracing::debug_span;

#[derive(Clone, Debug, Default)]
pub struct Config {
    pub allow_discovery: NameMatch,
}

#[derive(Clone, Debug)]
struct Allow(NameMatch);

#[derive(Clone, Debug)]
struct HttpClientInfo {
    target: Option<NameAddr>,
    client: ClientInfo,
    version: http::Version,
}

#[derive(Clone, Debug)]
struct RouteHttp(HttpClientInfo);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct HttpTarget {
    target: NameAddr,
    version: http::Version,
}

#[derive(Debug)]
struct TcpGatewayUnimplemented(());

#[derive(Debug, Default)]
struct RefusedNoTarget(());

#[allow(clippy::clippy::too_many_arguments)]
pub fn stack<I, O, OSvc, P>(
    Config { allow_discovery }: Config,
    inbound: &ProxyConfig,
    outbound: O,
    profiles: P,
    local_id: Option<tls::LocalId>,
    metrics: &metrics::Proxy,
    span_sink: Option<mpsc::Sender<oc::Span>>,
    drain: drain::Watch,
) -> impl svc::NewService<
    GatewayConnection,
    Service = impl tower::Service<I, Response = (), Error = impl Into<Error>, Future = impl Send>
                  + Send
                  + 'static,
> + Clone
       + Send
where
    I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + Send + Sync + Unpin + 'static,
    P: profiles::GetProfile<NameAddr> + Clone + Send + Sync + Unpin + 'static,
    P::Future: Send + 'static,
    P::Error: Send,
    O: svc::NewService<outbound::http::Logical, Service = OSvc>,
    O: Clone + Send + Sync + Unpin + 'static,
    OSvc: tower::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>
        + Send
        + 'static,
    OSvc::Error: Into<Error>,
    OSvc::Future: Send + 'static,
{
    let http_server = {
        // Cache an HTTP gateway service for each destination and HTTP version.
        //
        // The destination is determined from the transport header or, if none was
        // present, the request URI.
        //
        // The client's ID is set as a request extension, as required by the
        // gateway. This permits gateway services (and profile resolutions) to be
        // cached per target, shared across clients.
        let gateway = svc::stack(NewGateway::new(outbound, local_id))
            .push(profiles::discover::layer(profiles, Allow(allow_discovery)))
            .instrument(|h: &HttpTarget| debug_span!("gateway", target = %h.target, v = %h.version))
            .push_on_response(
                svc::layers()
                    .push(svc::layer::mk(svc::SpawnReady::new))
                    .push(
                        metrics
                            .stack
                            .layer(metrics::StackLabels::inbound("http", "gateway")),
                    )
                    .push(svc::FailFast::layer("Gateway", inbound.dispatch_timeout))
                    .push_spawn_buffer(inbound.buffer_capacity),
            )
            .push_cache(inbound.cache_max_idle_age)
            .push_on_response(
                svc::layers()
                    .push(http::Retain::layer())
                    .push(http::BoxResponse::layer()),
            )
            .push(svc::NewRouter::layer(RouteHttp))
            .push_http_insert_target::<tls::ClientId>()
            .into_inner();

        inbound::http::server(&inbound, gateway, metrics, span_sink, drain)
    };

    // As gateway connctions are received, dispatch HTTP connections to the HTTP
    // gateway. If no target was provided AND no protocol, then we attempt HTTP
    // detection (for legacy clients).
    //
    // TODO: Handle TCP gateway traffic when a target is set without a protocol.
    svc::stack(http_server.clone())
        .push_on_response(svc::layers().push_map_target(io::EitherIo::Left))
        .push_switch(
            |GatewayConnection {
                 target,
                 protocol,
                 client,
             }| match (protocol, target) {
                (Some(proto), Some(target)) => Ok(svc::Either::A(HttpClientInfo {
                    target: Some(target),
                    version: match proto {
                        SessionProtocol::Http1 => http::Version::Http1,
                        SessionProtocol::Http2 => http::Version::H2,
                    },
                    client,
                })),
                (None, None) => Ok(svc::Either::B(client)),
                (None, Some(_)) => Err(Error::from(TcpGatewayUnimplemented(()))),
                (Some(_), None) => Err(Error::from(RefusedNoTarget(()))),
            },
            svc::stack(http_server)
                .push_on_response(svc::layers().push_map_target(io::EitherIo::Right))
                .push_map_target(
                    |(version, client): (http::Version, ClientInfo)| HttpClientInfo {
                        target: None,
                        version,
                        client,
                    },
                )
                .push(svc::UnwrapOr::layer(
                    svc::Fail::<_, RefusedNoTarget>::default(),
                ))
                .push(detect::NewDetectService::timeout(
                    inbound.detect_protocol_timeout,
                    http::DetectHttp::default(),
                ))
                .into_inner(),
        )
        .into_inner()
}

impl svc::stack::Predicate<HttpTarget> for Allow {
    type Request = NameAddr;

    fn check(&mut self, t: HttpTarget) -> Result<NameAddr, Error> {
        // The service name needs to exist in the configured set of suffixes.
        if self.0.matches(t.target.name()) {
            Ok(t.target)
        } else {
            Err(discovery_rejected().into())
        }
    }
}

// === impl RouteHttpGatewayTarget ===

impl<B> svc::stack::RecognizeRoute<http::Request<B>> for RouteHttp {
    type Key = HttpTarget;

    fn recognize(&self, req: &http::Request<B>) -> Result<Self::Key, Error> {
        let version = req.version().try_into()?;

        if let Some(target) = self.0.target.clone() {
            return Ok(HttpTarget { target, version });
        }

        if let Some(a) = req.uri().authority() {
            let target = NameAddr::from_authority_with_default_port(a, 80)?;
            return Ok(HttpTarget { target, version });
        }

        Err(RefusedNoTarget(()).into())
    }
}

// === impl HttpClientInfo ===

impl Param<http::normalize_uri::DefaultAuthority> for HttpClientInfo {
    fn param(&self) -> http::normalize_uri::DefaultAuthority {
        http::normalize_uri::DefaultAuthority(self.target.as_ref().map(NameAddr::as_http_authority))
    }
}

impl Param<http::Version> for HttpClientInfo {
    fn param(&self) -> http::Version {
        self.version
    }
}

impl Param<tls::ClientId> for HttpClientInfo {
    fn param(&self) -> tls::ClientId {
        self.client.client_id.clone()
    }
}

// === impl TcpGatewayUnimplemented ===

impl std::fmt::Display for TcpGatewayUnimplemented {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TCP gateway support is not yet implemented")
    }
}

impl std::error::Error for TcpGatewayUnimplemented {}

// === impl RefusedNoTarget ===

impl std::fmt::Display for RefusedNoTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "A named target must be provided on gateway connections")
    }
}

impl std::error::Error for RefusedNoTarget {}
