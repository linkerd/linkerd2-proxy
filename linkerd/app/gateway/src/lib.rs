#![deny(warnings, rust_2018_idioms)]
#![allow(clippy::inconsistent_struct_constructor)]

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
    transport_header::SessionProtocol,
    Error, NameAddr, NameMatch, Never,
};
use linkerd_app_inbound::{
    direct::{ClientInfo, GatewayConnection, GatewayTransportHeader},
    Inbound,
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
}

#[derive(Clone, Debug)]
struct HttpTransportHeader {
    target: NameAddr,
    client: ClientInfo,
    version: http::Version,
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
) -> svc::BoxNewService<GatewayConnection, svc::BoxService<I, (), Error>>
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
        detect_protocol_timeout,
        dispatch_timeout,
        ..
    } = inbound.config().proxy.clone();
    let local_id = inbound.runtime().identity.as_ref().map(|l| l.id().clone());

    // For each gatewayed connection that is *not* HTTP, use the target from the
    // transport header to lookup a service profile. If the profile includes a
    // resolvable service name, then continue with TCP endpoint resolution,
    // balancing, and forwarding. An invalid original destination address is
    // used so that service discovery is *required* to provide a valid endpoint.
    //
    // TODO: We should use another target type that actually reflects
    // reality. But the outbound stack is currently pretty tightly
    // coupled to its target types.
    let tcp = outbound
        .clone()
        .push_tcp_endpoint()
        .push_tcp_logical(resolve.clone())
        .into_stack()
        .push_request_filter(
            |(p, _): (Option<profiles::Receiver>, _)| -> Result<_, Error> {
                let profile = p.ok_or_else(|| {
                    DiscoveryRejected::new("no profile discovered for gateway target")
                })?;
                let logical_addr = profile.borrow().addr.clone().ok_or_else(|| {
                    DiscoveryRejected::new(
                        "profile for gateway target does not have a logical address",
                    )
                })?;
                Ok(outbound::tcp::Logical {
                    profile,
                    protocol: (),
                    logical_addr,
                })
            },
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
        .push_on_response(
            svc::layers()
                .push(
                    inbound
                        .runtime()
                        .metrics
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
    let http = outbound
        .push_tcp_endpoint()
        .push_http_endpoint()
        .push_http_logical(resolve)
        .into_stack()
        .push(NewGateway::layer(local_id))
        .push(profiles::discover::layer(profiles, move |t: HttpTarget| {
            if allow_discovery.matches(t.target.name()) {
                Ok(profiles::LookupAddr(t.target.into()))
            } else {
                Err(RefusedNotResolved(t.target))
            }
        }))
        .instrument(|h: &HttpTarget| debug_span!("gateway", target = %h.target, v = %h.version))
        .push_on_response(
            svc::layers()
                .push(
                    inbound
                        .runtime()
                        .metrics
                        .stack
                        .layer(metrics::StackLabels::inbound("http", "gateway")),
                )
                .push(svc::layer::mk(svc::SpawnReady::new))
                .push(svc::FailFast::layer("Gateway", dispatch_timeout))
                .push_spawn_buffer(buffer_capacity),
        )
        .push_cache(cache_max_idle_age)
        .push_on_response(
            svc::layers()
                .push(http::Retain::layer())
                .push(http::BoxResponse::layer()),
        )
        .push(svc::BoxNewService::layer());

    // When handling gateway connections from older clients that do not
    // support the transport header, do protocol detection and route requests
    // based on each request's URI.
    //
    // Non-HTTP connections are refused.
    let legacy_http = inbound
        .clone()
        .with_stack(
            http.clone()
                .push(svc::NewRouter::layer(RouteHttp))
                .push_http_insert_target::<tls::ClientId>(),
        )
        .push_http_server()
        .into_stack()
        .push(svc::Filter::<ClientInfo, _>::layer(HttpLegacy::try_from))
        .push(svc::BoxNewService::layer())
        .push(detect::NewDetectService::layer(
            detect_protocol_timeout,
            http::DetectHttp::default(),
        ));

    // When a transported connection is received, use the header's target to
    // drive routing.
    inbound
        .with_stack(
            // A router is needed so that we use each request's HTTP version
            // (i.e. after serverside orig-proto downgrading).
            http.push(svc::NewRouter::layer(RouteHttp))
                .push_http_insert_target::<tls::ClientId>(),
        )
        .push_http_server()
        .into_stack()
        .push_on_response(svc::BoxService::layer())
        .push(svc::BoxNewService::layer())
        .push_switch(
            |GatewayTransportHeader {
                 target,
                 protocol,
                 client,
             }| match protocol {
                Some(proto) => Ok(svc::Either::A(HttpTransportHeader {
                    target,
                    client,
                    version: match proto {
                        SessionProtocol::Http1 => http::Version::Http1,
                        SessionProtocol::Http2 => http::Version::H2,
                    },
                })),
                None => Ok::<_, Never>(svc::Either::B(target)),
            },
            tcp.push_on_response(svc::BoxService::layer())
                .push(svc::BoxNewService::layer())
                .into_inner(),
        )
        .push_switch(
            |gw| match gw {
                GatewayConnection::TransportHeader(t) => Ok::<_, Never>(svc::Either::A(t)),
                GatewayConnection::Legacy(c) => Ok(svc::Either::B(c)),
            },
            legacy_http.into_inner(),
        )
        .push_on_response(svc::BoxService::layer())
        .push(svc::BoxNewService::layer())
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

// === impl HttpLegacy ===

impl<E: Into<Error>> TryFrom<(Result<Option<http::Version>, E>, ClientInfo)> for HttpLegacy {
    type Error = Error;

    fn try_from(
        (version, client): (Result<Option<http::Version>, E>, ClientInfo),
    ) -> Result<Self, Self::Error> {
        match version {
            Ok(Some(version)) => Ok(Self { version, client }),
            Ok(None) => Err(RefusedNoTarget(()).into()),
            Err(e) => Err(e.into()),
        }
    }
}

impl From<(http::Version, ClientInfo)> for HttpLegacy {
    fn from((version, client): (http::Version, ClientInfo)) -> Self {
        Self { version, client }
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

impl Param<tls::ClientId> for HttpLegacy {
    fn param(&self) -> tls::ClientId {
        self.client.client_id.clone()
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

        if let Some(a) = req.uri().authority() {
            let target = NameAddr::from_authority_with_default_port(a, 80)?;
            return Ok(HttpTarget { target, version });
        }

        Err(RefusedNoTarget(()).into())
    }
}
