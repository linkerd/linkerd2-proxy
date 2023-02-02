#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

mod gateway;
#[cfg(test)]
mod tests;

use self::gateway::NewHttpGateway;
use linkerd_app_core::{
    identity, io, metrics, profiles,
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
        http,
    },
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

#[derive(Clone, Debug, Default)]
pub struct Config {
    pub allow_discovery: NameMatch,
}

/// A target type describing an HTTP gateway connection from an inbound proxy,
/// including enough information to enforce inbound policy on the gatewayed
/// requests.
#[derive(Clone, Debug)]
struct InboundHttp {
    client: ClientInfo,
    inbound_policy: policy::AllowPolicy,
    outbound: OutboundHttp,
}

/// A target type describing outbound HTTP traffic.
#[derive(Clone, Debug)]
struct OutboundHttp {
    target: NameAddr,
    profile: Option<profiles::Receiver>,
    version: http::Version,
}

/// A target type describing an opaque gateway connection from an inbound proxy,
/// including enough informatiion to enforce inbound policy on the gatewayed
/// connection.
#[derive(Clone, Debug)]
struct Opaque {
    target: NameAddr,
    client: ClientInfo,
    inbound_policy: policy::AllowPolicy,
    profile: profiles::Receiver,
}

/// Implements `svc::router::SelectRoute` for outbound HTTP requests. An
/// `OutboundHttp` target is returned for each request using the request's HTTP
/// version.
///
/// The request's HTTP version may not match the target's original HTTP version
/// when proxies use HTTP/2 to transport HTTP/1 requests.
#[derive(Clone, Debug)]
struct ByRequestVersion(OutboundHttp);

#[derive(Debug, Default, Error)]
#[error("a named target must be provided on gateway connections")]
struct RefusedNoTarget(());

#[derive(Debug, Error)]
#[error("the provided address could not be resolved: {}", self.0)]
struct RefusedNotResolved(NameAddr);

/// Builds a gateway between inbound and outbound proxy stacks.
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
    P: profiles::GetProfile<Error = Error>,
    R: Clone + Send + Sync + Unpin + 'static,
    R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>,
{
    let local_id = identity::LocalId(inbound.identity().name().clone());

    let opaque = {
        let stk = new_opaque(outbound.clone(), resolve.clone());
        svc::stack(stk)
            .push_map_target(|(_permit, opaque): (_, Opaque)| opaque)
            .push(inbound.authorize_tcp())
            .check_new_service::<Opaque, I>()
    };

    let http = {
        let stk = new_http(local_id, outbound.clone(), resolve);
        inbound
            .clone()
            .with_stack(
                svc::stack(stk)
                    .push_on_service(svc::LoadShed::layer())
                    .lift_new()
                    .push(svc::NewOneshotRoute::layer_via(
                        |(_permit, http): &(_, InboundHttp)| {
                            ByRequestVersion(http.outbound.clone())
                        },
                    ))
                    .push(inbound.authorize_http())
                    .push_http_insert_target::<tls::ClientId>()
                    .into_inner(),
            )
            .push_http_server()
            .into_stack()
            .check_new_service::<InboundHttp, I>()
    };

    let protocol = http
        .check_new_service::<InboundHttp, I>()
        .push_switch(
            |(profile, gth): (Option<profiles::Receiver>, GatewayTransportHeader)| -> Result<_, Error> {
                if let Some(proto) = gth.protocol {
                    return Ok(svc::Either::A(InboundHttp {
                        client: gth.client,
                        inbound_policy: gth.policy,
                        outbound: OutboundHttp {
                            profile,
                            target: gth.target,
                            version: match proto {
                                SessionProtocol::Http1 => http::Version::Http1,
                                SessionProtocol::Http2 => http::Version::H2,
                            },
                        },
                    }));
                }

                let profile = profile.ok_or_else(|| RefusedNotResolved(gth.target.clone()))?;
                Ok(svc::Either::B(Opaque {
                    profile,
                    target: gth.target,
                    client: gth.client,
                    inbound_policy: gth.policy,
                }))
            },
            opaque.into_inner(),
        )
        .push_on_service(svc::BoxService::layer())
        .check_new_service::<(Option<profiles::Receiver>, GatewayTransportHeader), I>();

    let discover = protocol
        .clone()
        .check_new_service::<(Option<profiles::Receiver>, GatewayTransportHeader), I>()
        .lift_new_with_target()
        .push_new_cached_discover(
            profiles.into_service(),
            outbound.config().discovery_idle_timeout,
        )
        .push_filter(move |gth: GatewayTransportHeader| {
            if !allow_discovery.matches(gth.target.name()) {
                return Err(RefusedNotResolved(gth.target));
            }
            Ok(gth)
        })
        .check_new_service::<GatewayTransportHeader, I>();

    discover
        .push_on_service(
            svc::layers()
                .push(
                    outbound
                        .stack_metrics()
                        .layer(metrics::StackLabels::outbound("tcp", "gateway")),
                )
                .push(svc::BoxService::layer()),
        )
        .push(svc::ArcNewService::layer())
        .check_new_service::<GatewayTransportHeader, I>()
        .into_inner()
}

/// Builds an outbound HTTP stack.
///
/// A gateway-specififc module is inserted to requests from looping through
/// gateways. Discovery errors are lifted into the HTTP stack so that individual
/// requests are failed with an HTTP-level error repsonse.
fn new_http<O, R>(
    local_id: identity::LocalId,
    outbound: Outbound<O>,
    resolve: R,
) -> svc::ArcNewService<
    OutboundHttp,
    impl svc::Service<
        http::Request<http::BoxBody>,
        Response = http::Response<http::BoxBody>,
        Error = Error,
        Future = impl Send,
    >,
>
where
    O: Clone + Send + Sync + Unpin + 'static,
    O: svc::MakeConnection<outbound::tcp::Connect, Metadata = Local<ClientAddr>, Error = io::Error>,
    O::Connection: Send + Unpin,
    O::Future: Send + Unpin + 'static,
    R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>,
{
    outbound
        .clone()
        .push_tcp_endpoint()
        .push_http_endpoint()
        .push_http_concrete(resolve)
        .push_http_logical()
        .into_stack()
        .push_switch(
            Ok::<_, Infallible>,
            outbound
                .push_tcp_endpoint()
                .push_http_endpoint()
                .into_inner(),
        )
        .push(NewHttpGateway::layer(local_id))
        .push(svc::ArcNewService::layer())
        .check_new::<OutboundHttp>()
        .into_inner()
}

/// Builds an outbound opaque stack.
///
/// Requires that the connection targets either a logical service or a known
/// endpoint.
fn new_opaque<I, O, R>(
    outbound: Outbound<O>,
    resolve: R,
) -> svc::ArcNewService<
    Opaque,
    impl svc::Service<I, Response = (), Error = Error, Future = impl Send>,
>
where
    I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + fmt::Debug + Send + Sync + Unpin + 'static,
    O: Clone + Send + Sync + Unpin + 'static,
    O: svc::MakeConnection<outbound::tcp::Connect, Metadata = Local<ClientAddr>, Error = io::Error>,
    O::Connection: Send + Unpin,
    O::Future: Send + Unpin + 'static,
    R: Clone + Send + Sync + Unpin + 'static,
    R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>,
{
    let logical = outbound
        .clone()
        .push_tcp_endpoint()
        .push_opaque_concrete(resolve)
        .push_opaque_logical();
    let endpoint = outbound
        .clone()
        .push_tcp_endpoint()
        .push_opaque_forward()
        .into_stack();
    let inbound_ips = outbound.config().inbound_ips.clone();
    endpoint
        .push_switch(
            move |opaque: Opaque| -> Result<_, Error> {
                if let Some((addr, metadata)) = opaque.profile.endpoint() {
                    return Ok(svc::Either::A(outbound::tcp::Endpoint::from_metadata(
                        addr,
                        metadata,
                        tls::NoClientTls::NotProvidedByServiceDiscovery,
                        opaque.profile.is_opaque_protocol(),
                        &inbound_ips,
                    )));
                }

                let logical_addr = opaque
                    .profile
                    .logical_addr()
                    .ok_or(RefusedNotResolved(opaque.target))?;
                Ok(svc::Either::B(outbound::tcp::Logical {
                    profile: opaque.profile,
                    logical_addr,
                    protocol: (),
                }))
            },
            logical.into_inner(),
        )
        .push(svc::ArcNewService::layer())
        .check_new_service::<Opaque, I>()
        .into_inner()
}

// === impl OutboundHttp ===

impl Param<http::Version> for OutboundHttp {
    fn param(&self) -> http::Version {
        self.version
    }
}

// === impl InboundHttp ===

impl Param<http::normalize_uri::DefaultAuthority> for InboundHttp {
    fn param(&self) -> http::normalize_uri::DefaultAuthority {
        http::normalize_uri::DefaultAuthority(Some(self.outbound.target.as_http_authority()))
    }
}

impl Param<Option<identity::Name>> for InboundHttp {
    fn param(&self) -> Option<identity::Name> {
        Some(self.client.client_id.clone().0)
    }
}

impl Param<http::Version> for InboundHttp {
    fn param(&self) -> http::Version {
        self.outbound.version
    }
}

impl Param<tls::ClientId> for InboundHttp {
    fn param(&self) -> tls::ClientId {
        self.client.client_id.clone()
    }
}

impl Param<OrigDstAddr> for InboundHttp {
    fn param(&self) -> OrigDstAddr {
        self.client.local_addr
    }
}

impl Param<Remote<ClientAddr>> for InboundHttp {
    fn param(&self) -> Remote<ClientAddr> {
        self.client.client_addr
    }
}

impl Param<tls::ConditionalServerTls> for InboundHttp {
    fn param(&self) -> tls::ConditionalServerTls {
        tls::ConditionalServerTls::Some(tls::ServerTls::Established {
            client_id: Some(self.client.client_id.clone()),
            negotiated_protocol: self.client.alpn.clone(),
        })
    }
}

impl Param<policy::AllowPolicy> for InboundHttp {
    fn param(&self) -> policy::AllowPolicy {
        self.inbound_policy.clone()
    }
}

impl Param<policy::ServerLabel> for InboundHttp {
    fn param(&self) -> policy::ServerLabel {
        self.inbound_policy.server_label()
    }
}

// === impl ByRequestVersion ===

impl<B> svc::router::SelectRoute<http::Request<B>> for ByRequestVersion {
    type Key = OutboundHttp;
    type Error = Error;

    fn select(&self, req: &http::Request<B>) -> Result<Self::Key, Error> {
        Ok(OutboundHttp {
            version: req.version().try_into()?,
            ..self.0.clone()
        })
    }
}

// === impl Opaque ===

impl Param<policy::AllowPolicy> for Opaque {
    fn param(&self) -> policy::AllowPolicy {
        self.inbound_policy.clone()
    }
}

impl Param<OrigDstAddr> for Opaque {
    fn param(&self) -> OrigDstAddr {
        self.client.local_addr
    }
}

impl Param<Remote<ClientAddr>> for Opaque {
    fn param(&self) -> Remote<ClientAddr> {
        self.client.client_addr
    }
}

impl Param<tls::ConditionalServerTls> for Opaque {
    fn param(&self) -> tls::ConditionalServerTls {
        tls::ConditionalServerTls::Some(tls::ServerTls::Established {
            client_id: Some(self.client.client_id.clone()),
            negotiated_protocol: self.client.alpn.clone(),
        })
    }
}
