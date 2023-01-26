#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

mod gateway;
#[cfg(test)]
mod tests;

use self::gateway::NewGateway;
use linkerd_app_core::{
    identity, io, metrics,
    profiles::{self, DiscoveryRejected},
    proxy::{api_resolve::Metadata, core::Resolve, http},
    svc::{self, Param},
    tls,
    transport::{ClientAddr, Local, OrigDstAddr, Remote},
    transport_header::SessionProtocol,
    Error, Infallible, NameAddr, NameMatch,
};
use linkerd_app_inbound::{
    direct::{ClientInfo, GatewayTransportHeader},
    policy, Inbound,
};
use linkerd_app_outbound::{self as outbound, Outbound};
use std::fmt;
use thiserror::Error;
use tracing::debug_span;

#[derive(Clone, Debug, Default)]
pub struct Config {
    pub allow_discovery: NameMatch,
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

pub fn stack<I, O, P, R>(
    Config { allow_discovery }: Config,
    inbound: Inbound<()>,
    outbound: Outbound<O>,
    profiles: P,
    resolve: R,
) -> svc::ArcNewTcp<GatewayTransportHeader, I>
where
    I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + fmt::Debug + Send + Sync + Unpin + 'static,
    O: Clone + Send + Sync + Unpin + 'static,
    O: svc::MakeConnection<outbound::tcp::Connect, Metadata = Local<ClientAddr>, Error = io::Error>,
    O::Connection: Send + Unpin,
    O::Future: Send + Unpin + 'static,
    P: profiles::GetProfile + Clone + Send + Sync + Unpin + 'static,
    P::Future: Send + 'static,
    R: Clone + Send + Sync + Unpin + 'static,
    R: Resolve<outbound::tcp::Concrete, Endpoint = Metadata, Error = Error>,
    <R as Resolve<outbound::tcp::Concrete>>::Resolution: Send,
    <R as Resolve<outbound::tcp::Concrete>>::Future: Send + Unpin,
    R: Resolve<outbound::http::Concrete, Endpoint = Metadata, Error = Error>,
    <R as Resolve<outbound::http::Concrete>>::Resolution: Send,
    <R as Resolve<outbound::http::Concrete>>::Future: Send + Unpin,
{
    let inbound_config = inbound.config().clone();
    let local_id = identity::LocalId(inbound.identity().name().clone());

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
    let opaque = {
        let logical = outbound
            .clone()
            .push_tcp_endpoint()
            .push_tcp_concrete(resolve.clone())
            .push_tcp_logical();
        let endpoint = outbound
            .clone()
            .push_tcp_endpoint()
            .push_tcp_forward()
            .into_stack();
        let inbound_ips = outbound.config().inbound_ips.clone();
        let stack = endpoint.push_switch(
            move |profile: Option<profiles::Receiver>| -> Result<_, Error> {
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
        );

        let allow = allow_discovery.clone();
        let discover = stack
            .clone()
            .check_new::<Option<profiles::Receiver>>()
            .lift_new()
            .push(profiles::Discover::layer(profiles.clone()))
            .check_new_service::<profiles::LookupAddr, I>()
            .push_switch(
                move |addr: NameAddr| -> Result<_, Infallible> {
                    if !allow.matches(addr.name()) {
                        return Ok(svc::Either::B(None));
                    }
                    Ok(svc::Either::A(profiles::LookupAddr(addr.into())))
                },
                stack,
            )
            .check_new_service::<NameAddr, I>();

        discover
            .push_on_service(
                svc::layers().push(
                    inbound
                        .proxy_metrics()
                        .stack
                        .layer(metrics::StackLabels::inbound("tcp", "gateway")),
                ),
            )
            .push(svc::NewQueue::layer_fixed(
                outbound.config().tcp_connection_buffer,
            ))
            .push_idle_cache(outbound.config().discovery_idle_timeout)
            .check_new_service::<NameAddr, I>()
    };

    // Cache an HTTP gateway service for each destination and HTTP version.
    //
    // The client's ID is set as a request extension, as required by the
    // gateway. This permits gateway services (and profile resolutions) to be
    // cached per target, shared across clients.
    let http = {
        let endpoint = outbound.push_tcp_endpoint().push_http_endpoint();
        let stack = endpoint
            .clone()
            .push_http_concrete(resolve)
            .push_http_logical()
            .into_stack();

        let gateway = stack
            .push_switch(Ok::<_, Infallible>, endpoint.into_stack())
            .push(NewGateway::layer(local_id))
            .check_new::<(Option<profiles::Receiver>, HttpTarget)>();

        let discover = gateway
            .clone()
            .check_new::<(Option<profiles::Receiver>, HttpTarget)>()
            .lift_new_with_target()
            .check_new_new::<HttpTarget, Option<profiles::Receiver>>()
            .push(profiles::Discover::layer(profiles))
            .push_switch(
                move |t: HttpTarget| -> Result<_, Infallible> {
                    if !allow_discovery.matches(t.target.name()) {
                        return Ok(svc::Either::B((None, t)));
                    }
                    Ok(svc::Either::A(t))
                },
                gateway,
            );

        discover
            .instrument(|h: &HttpTarget| debug_span!("gateway", target = %h.target))
            .push_on_service(
                svc::layers().push(
                    inbound
                        .proxy_metrics()
                        .stack
                        .layer(metrics::StackLabels::inbound("http", "gateway")),
                ),
            )
            .push(svc::NewQueue::layer_fixed(
                inbound_config.http_request_buffer,
            ))
            .push_idle_cache(inbound_config.discovery_idle_timeout)
            .push_on_service(
                svc::layers()
                    .push(http::Retain::layer())
                    .push(http::BoxResponse::layer()),
            )
            .push(svc::ArcNewService::layer())
    };

    // When a transported connection is received, use the header's target to
    // drive routing.
    let http_server = inbound
        .clone()
        .with_stack(
            // A router is needed so that we use each request's HTTP version
            // (i.e. after server-side orig-proto downgrading).
            http.push_on_service(svc::LoadShed::layer())
                .lift_new()
                .push(svc::NewOneshotRoute::layer_via(
                    |(_, target): &(_, HttpTransportHeader)| RouteHttp(target.clone()),
                ))
                .push(inbound.authorize_http())
                .push_http_insert_target::<tls::ClientId>(),
        )
        .push_http_server()
        .into_stack();

    http_server
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
            opaque
                .push_map_target(|(_permit, gth): (_, GatewayTransportHeader)| gth.target)
                .push(inbound.authorize_tcp())
                .check_new_service::<GatewayTransportHeader, I>()
                .push_on_service(svc::BoxService::layer())
                .push(svc::ArcNewService::layer())
                .into_inner(),
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

// === impl HttpTarget ===

impl Param<profiles::LookupAddr> for HttpTarget {
    fn param(&self) -> profiles::LookupAddr {
        profiles::LookupAddr(self.target.clone().into())
    }
}

impl Param<http::Version> for HttpTarget {
    fn param(&self) -> http::Version {
        self.version
    }
}

// === impl RouteHttp ===

impl<B> svc::router::SelectRoute<http::Request<B>> for RouteHttp<HttpTransportHeader> {
    type Key = HttpTarget;
    type Error = Error;

    fn select(&self, req: &http::Request<B>) -> Result<Self::Key, Error> {
        let target = self.0.target.clone();
        let version = req.version().try_into()?;
        Ok(HttpTarget { target, version })
    }
}
