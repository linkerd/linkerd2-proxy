#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]

mod gateway;
#[cfg(test)]
mod tests;

use self::gateway::NewGateway;
use linkerd_app_core::{
    config::ProxyConfig,
    detect, identity, io, metrics,
    profiles::{self, DiscoveryRejected},
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
        http,
    },
    svc::{self, Param},
    tls,
    transport::{ClientAddr, OrigDstAddr, Remote},
    transport_header::SessionProtocol,
    Error, Infallible, NameAddr, NameMatch,
};
use linkerd_app_inbound::{
    direct::{ClientInfo, GatewayConnection, GatewayTransportHeader, Legacy},
    policy, Inbound,
};
use linkerd_app_outbound::{self as outbound, Outbound};
use std::{
    convert::{TryFrom, TryInto},
    fmt,
};
use thiserror::Error;
use tracing::debug_span;

#[derive(Clone, Debug, Default)]
pub struct Config {
    pub allow_discovery: NameMatch,
}

#[derive(Clone, Debug)]
struct HttpLegacy {
    client: ClientInfo,
    version: http::Version,
    policy: policy::AllowPolicy,
}

#[derive(Clone, Debug)]
struct HttpTransportHeader {
    target: NameAddr,
    client: ClientInfo,
    version: http::Version,
    policy: policy::AllowPolicy,
}

#[derive(Clone, Debug)]
struct RouteHttp<T>(T);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct HttpTarget {
    target: NameAddr,
    version: http::Version,
}

#[derive(Debug, Default, Error)]
#[error("a named target must be provided on gateway connections")]
struct RefusedNoTarget(());

#[derive(Debug, Error)]
#[error("the provided address could not be resolved: {}", self.0)]
struct RefusedNotResolved(NameAddr);

#[allow(clippy::too_many_arguments)]
pub fn stack<I, O, P, R>(
    Config { allow_discovery }: Config,
    inbound: Inbound<()>,
    outbound: Outbound<O>,
    profiles: P,
    resolve: R,
) -> svc::BoxNewTcp<GatewayConnection, I>
where
    I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + fmt::Debug + Send + Sync + Unpin + 'static,
    O: Clone + Send + Sync + Unpin + 'static,
    O: svc::Service<outbound::tcp::Connect, Error = io::Error>,
    O::Response:
        io::AsyncRead + io::AsyncWrite + tls::HasNegotiatedProtocol + Send + Unpin + 'static,
    O::Future: Send + Unpin + 'static,
    P: profiles::GetProfile<profiles::LookupAddr> + Clone + Send + Sync + Unpin + 'static,
    P::Future: Send + 'static,
    P::Error: Send,
    R: Clone + Send + Sync + Unpin + 'static,
    R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>,
    R::Resolution: Send,
    R::Future: Send + Unpin,
{
    let ProxyConfig {
        buffer_capacity,
        cache_max_idle_age,
        dispatch_timeout,
        ..
    } = inbound.config().proxy.clone();
    let local_id = inbound.identity().map(|l| l.id().clone());

    // For each gatewayed connection that is *not* HTTP, use the target from the
    // transport header to lookup a service profile. If the profile includes a
    // resolvable service name, then continue with TCP endpoint resolution,
    // balancing, and forwarding. If the profile includes an endpoint instead
    // of a logical address, then connect to endpoint directly and avoid
    // balancing.
    //
    // TODO: We should use another target type that actually reflects
    // reality. But the outbound stack is currently pretty tightly
    // coupled to its target types.
    let logical = outbound
        .clone()
        .push_tcp_endpoint()
        .push_tcp_logical(resolve.clone());
    let endpoint = outbound
        .clone()
        .push_tcp_endpoint()
        .push_tcp_forward()
        .into_stack();
    let inbound_ips = outbound.config().inbound_ips.clone();
    let tcp = endpoint
        .push_switch(
            move |(profile, _): (Option<profiles::Receiver>, _)| -> Result<_, Error> {
                let profile = profile.ok_or_else(|| {
                    DiscoveryRejected::new("no profile discovered for gateway target")
                })?;

                if let Some((addr, metadata)) = profile.endpoint() {
                    return Ok(svc::Either::A(outbound::tcp::Endpoint::from_metadata(
                        addr,
                        metadata,
                        tls::NoClientTls::NotProvidedByServiceDiscovery,
                        profile.is_opaque_protocol(),
                        &inbound_ips,
                    )));
                }

                let logical_addr = profile.logical_addr().ok_or_else(|| {
                    DiscoveryRejected::new(
                        "profiles must have either an endpoint or a logical address",
                    )
                })?;

                Ok(svc::Either::B(outbound::tcp::Logical {
                    profile,
                    protocol: (),
                    logical_addr,
                }))
            },
            logical.into_inner(),
        )
        .push(profiles::discover::layer(profiles.clone(), {
            let allow = allow_discovery.clone();
            move |addr: NameAddr| {
                if allow.matches(addr.name()) {
                    Ok(profiles::LookupAddr(addr.into()))
                } else {
                    Err(RefusedNotResolved(addr))
                }
            }
        }))
        .push_on_service(
            svc::layers()
                .push(
                    inbound
                        .proxy_metrics()
                        .stack
                        .layer(metrics::StackLabels::inbound("tcp", "gateway")),
                )
                .push(svc::layer::mk(svc::SpawnReady::new))
                .push(svc::FailFast::layer("TCP Gateway", dispatch_timeout))
                .push_spawn_buffer(buffer_capacity),
        )
        .push_cache(cache_max_idle_age)
        .check_new_service::<NameAddr, I>();

    // Cache an HTTP gateway service for each destination and HTTP version.
    //
    // The client's ID is set as a request extension, as required by the
    // gateway. This permits gateway services (and profile resolutions) to be
    // cached per target, shared across clients.
    let endpoint = outbound.push_tcp_endpoint().push_http_endpoint();
    let http = endpoint
        .clone()
        .push_http_logical(resolve)
        .into_stack()
        .push_switch(Ok::<_, Infallible>, endpoint.into_stack())
        .push(NewGateway::layer(local_id))
        .push(profiles::discover::layer(profiles, move |t: HttpTarget| {
            if allow_discovery.matches(t.target.name()) {
                Ok(profiles::LookupAddr(t.target.into()))
            } else {
                Err(RefusedNotResolved(t.target))
            }
        }))
        .instrument(|h: &HttpTarget| debug_span!("gateway", target = %h.target, v = %h.version))
        .push_on_service(
            svc::layers()
                .push(
                    inbound
                        .proxy_metrics()
                        .stack
                        .layer(metrics::StackLabels::inbound("http", "gateway")),
                )
                .push(svc::layer::mk(svc::SpawnReady::new))
                .push(svc::FailFast::layer("Gateway", dispatch_timeout))
                .push_spawn_buffer(buffer_capacity),
        )
        .push_cache(cache_max_idle_age)
        .push_on_service(
            svc::layers()
                .push(http::Retain::layer())
                .push(http::BoxResponse::layer()),
        )
        .push(svc::ArcNewService::layer());

    // When handling gateway connections from older clients that do not
    // support the transport header, do protocol detection and route requests
    // based on each request's URI.
    //
    // Non-HTTP connections are refused.
    let legacy_http = inbound
        .clone()
        .with_stack(
            http.clone()
                .push(svc::NewRouter::layer(|(_, target)| RouteHttp(target)))
                .push(inbound.authorize_http())
                .push_http_insert_target::<tls::ClientId>(),
        )
        .push_http_server()
        .into_stack()
        .push(svc::Filter::<ClientInfo, _>::layer(HttpLegacy::try_from))
        .push(svc::ArcNewService::layer())
        .push(detect::NewDetectService::layer(
            inbound.config().proxy.detect_http(),
        ));

    // When a transported connection is received, use the header's target to
    // drive routing.
    inbound
        .clone()
        .with_stack(
            // A router is needed so that we use each request's HTTP version
            // (i.e. after server-side orig-proto downgrading).
            http.push(svc::NewRouter::layer(|(_, target)| RouteHttp(target)))
                .push(inbound.authorize_http())
                .push_http_insert_target::<tls::ClientId>(),
        )
        .push_http_server()
        .into_stack()
        .push_on_service(svc::BoxService::layer())
        .push(svc::ArcNewService::layer())
        .push_switch(
            |gth: GatewayTransportHeader| match gth.protocol {
                Some(proto) => Ok(svc::Either::A(HttpTransportHeader {
                    target: gth.target,
                    client: gth.client,
                    policy: gth.policy,
                    version: match proto {
                        SessionProtocol::Http1 => http::Version::Http1,
                        SessionProtocol::Http2 => http::Version::H2,
                    },
                })),
                None => Ok::<_, Infallible>(svc::Either::B(gth)),
            },
            tcp.push_map_target(|(_permit, gth): (_, GatewayTransportHeader)| gth.target)
                .push(inbound.authorize_tcp())
                .check_new_service::<GatewayTransportHeader, I>()
                .push_on_service(svc::BoxService::layer())
                .push(svc::ArcNewService::layer())
                .into_inner(),
        )
        .push_switch(
            |gw| match gw {
                GatewayConnection::TransportHeader(t) => Ok::<_, Infallible>(svc::Either::A(t)),
                GatewayConnection::Legacy(c) => Ok(svc::Either::B(c)),
            },
            legacy_http.into_inner(),
        )
        .push_on_service(svc::BoxService::layer())
        .push(svc::ArcNewService::layer())
        .into_inner()
}

// === impl HttpTransportHeader ===

impl Param<http::normalize_uri::DefaultAuthority> for HttpTransportHeader {
    fn param(&self) -> http::normalize_uri::DefaultAuthority {
        http::normalize_uri::DefaultAuthority(Some(self.target.as_http_authority()))
    }
}

impl Param<Option<identity::Name>> for HttpTransportHeader {
    fn param(&self) -> Option<identity::Name> {
        Some(self.client.client_id.clone().0)
    }
}

impl Param<http::Version> for HttpTransportHeader {
    fn param(&self) -> http::Version {
        self.version
    }
}

impl Param<tls::ClientId> for HttpTransportHeader {
    fn param(&self) -> tls::ClientId {
        self.client.client_id.clone()
    }
}

impl Param<OrigDstAddr> for HttpTransportHeader {
    fn param(&self) -> OrigDstAddr {
        self.client.local_addr
    }
}

impl Param<Remote<ClientAddr>> for HttpTransportHeader {
    fn param(&self) -> Remote<ClientAddr> {
        self.client.client_addr
    }
}

impl Param<tls::ConditionalServerTls> for HttpTransportHeader {
    fn param(&self) -> tls::ConditionalServerTls {
        tls::ConditionalServerTls::Some(tls::ServerTls::Established {
            client_id: Some(self.client.client_id.clone()),
            negotiated_protocol: self.client.alpn.clone(),
        })
    }
}

impl Param<policy::AllowPolicy> for HttpTransportHeader {
    fn param(&self) -> policy::AllowPolicy {
        self.policy.clone()
    }
}

impl Param<policy::ServerLabel> for HttpTransportHeader {
    fn param(&self) -> policy::ServerLabel {
        self.policy.server_label()
    }
}

// === impl HttpLegacy ===

impl<E: Into<Error>> TryFrom<(Result<Option<http::Version>, E>, Legacy)> for HttpLegacy {
    type Error = Error;

    fn try_from(
        (version, gateway): (Result<Option<http::Version>, E>, Legacy),
    ) -> Result<Self, Self::Error> {
        match version {
            Ok(Some(version)) => Ok(Self {
                version,
                client: gateway.client,
                policy: gateway.policy,
            }),
            Ok(None) => Err(RefusedNoTarget(()).into()),
            Err(e) => Err(e.into()),
        }
    }
}

impl Param<http::normalize_uri::DefaultAuthority> for HttpLegacy {
    fn param(&self) -> http::normalize_uri::DefaultAuthority {
        http::normalize_uri::DefaultAuthority(None)
    }
}

impl Param<Option<identity::Name>> for HttpLegacy {
    fn param(&self) -> Option<identity::Name> {
        Some(self.client.client_id.clone().0)
    }
}

impl Param<http::Version> for HttpLegacy {
    fn param(&self) -> http::Version {
        self.version
    }
}

impl Param<OrigDstAddr> for HttpLegacy {
    fn param(&self) -> OrigDstAddr {
        self.client.local_addr
    }
}

impl Param<Remote<ClientAddr>> for HttpLegacy {
    fn param(&self) -> Remote<ClientAddr> {
        self.client.client_addr
    }
}

impl Param<tls::ClientId> for HttpLegacy {
    fn param(&self) -> tls::ClientId {
        self.client.client_id.clone()
    }
}

impl Param<tls::ConditionalServerTls> for HttpLegacy {
    fn param(&self) -> tls::ConditionalServerTls {
        tls::ConditionalServerTls::Some(tls::ServerTls::Established {
            client_id: Some(self.client.client_id.clone()),
            negotiated_protocol: self.client.alpn.clone(),
        })
    }
}

impl Param<policy::AllowPolicy> for HttpLegacy {
    fn param(&self) -> policy::AllowPolicy {
        self.policy.clone()
    }
}

impl Param<policy::ServerLabel> for HttpLegacy {
    fn param(&self) -> policy::ServerLabel {
        self.policy.server_label()
    }
}

// === impl RouteHttp ===

impl<B> svc::stack::RecognizeRoute<http::Request<B>> for RouteHttp<HttpTransportHeader> {
    type Key = HttpTarget;

    fn recognize(&self, req: &http::Request<B>) -> Result<Self::Key, Error> {
        let target = self.0.target.clone();
        let version = req.version().try_into()?;
        Ok(HttpTarget { target, version })
    }
}

impl<B> svc::stack::RecognizeRoute<http::Request<B>> for RouteHttp<HttpLegacy> {
    type Key = HttpTarget;

    fn recognize(&self, req: &http::Request<B>) -> Result<Self::Key, Error> {
        let version = req.version().try_into()?;
        let authority = req.uri().authority().ok_or(RefusedNoTarget(()))?;
        let target = NameAddr::from_authority_with_default_port(authority, 80)?;
        Ok(HttpTarget { target, version })
    }
}
