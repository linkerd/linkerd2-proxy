#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

mod gateway;
#[cfg(test)]
mod tests;

use self::gateway::NewHttpGateway;
use inbound::{GatewayAddr, GatewayDomainInvalid};
use linkerd_app_core::{
    identity, io, profiles,
    proxy::http,
    svc::{self, Param},
    tls,
    transport::addrs::*,
    transport_header::SessionProtocol,
    Error, NameAddr, NameMatch,
};
use linkerd_app_inbound::{self as inbound, Inbound};
use linkerd_app_outbound::{self as outbound, Outbound};
use std::{fmt::Debug, hash::Hash};
use thiserror::Error;

#[derive(Clone, Debug, Default)]
pub struct Config {
    pub allow_discovery: NameMatch,
}

#[derive(Clone)]
pub struct Gateway {
    config: Config,
    inbound: Inbound<()>,
    outbound: Outbound<()>,
}

#[derive(Clone, Debug)]
pub struct OutboundHttp {
    target: outbound::http::logical::Target,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Http<T> {
    version: http::Version,
    parent: outbound::Discovery<T>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Opaque<T>(outbound::Discovery<T>);

#[derive(Clone, Debug)]
pub struct HttpOutbound<T> {
    profile: profiles::Receiver,
    parent: Http<T>,
}

/// Implements `svc::router::SelectRoute` for outbound HTTP requests. An
/// `OutboundHttp` target is returned for each request using the request's HTTP
/// version.
///
/// The request's HTTP version may not match the target's original HTTP version
/// when proxies use HTTP/2 to transport HTTP/1 requests.
#[derive(Clone, Debug)]
struct ByRequestVersion<T>(T);

#[derive(Debug, Default, Error)]
#[error("a named target must be provided on gateway connections")]
struct RefusedNoTarget(());

#[derive(Debug, Error)]
#[error("the provided address could not be resolved: {}", self.0)]
struct RefusedNotResolved(NameAddr);

impl Gateway {
    fn new(config: Config, inbound: Inbound<()>, outbound: Outbound<()>) -> Self {
        Self {
            config,
            inbound,
            outbound,
        }
    }
}

impl Gateway {
    /// Builds a gateway between inbound and outbound proxy stacks.
    pub fn stack<T, I, P, O, H, OSvc, HSvc>(
        self,
        profiles: P,
        opaque: O,
        http: H,
    ) -> svc::ArcNewTcp<T, I>
    where
        // Target
        T: svc::Param<GatewayAddr>,
        T: svc::Param<inbound::policy::AllowPolicy>,
        T: svc::Param<Remote<ClientAddr>>,
        T: svc::Param<tls::ConditionalServerTls>,
        T: svc::Param<tls::ClientId>,
        T: svc::Param<Option<SessionProtocol>>,
        T: svc::Param<profiles::LookupAddr>,
        T: Clone + Send + Sync + Unpin + 'static,
        // Inbound socket
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + Debug + Send + Sync + Unpin + 'static,
        // Discovery
        P: profiles::GetProfile<Error = Error>,
        // Outbound opaque stack
        O: svc::NewService<Opaque<T>, Service = OSvc>,
        O: Clone + Send + Sync + Unpin + 'static,
        OSvc: svc::Service<I, Response = (), Error = Error>,
        OSvc: Send + Unpin + 'static,
        OSvc::Future: Send + 'static,
        // Outbound HTTP stack
        H: svc::NewService<HttpOutbound<T>, Service = HSvc>,
        H: Clone + Send + Sync + Unpin + 'static,
        HSvc: svc::Service<
            http::Request<http::BoxBody>,
            Response = http::Response<http::BoxBody>,
            Error = Error,
        >,
        HSvc: Send + Unpin + 'static,
        HSvc::Future: Send + 'static,
    {
        // XXX TODO THIS STACK PROBABLY NEEDS TO HANDLE CACHING LOAD BALANCERS,
        // etc.

        let tcp_http = {
            let http = svc::stack(http)
                .check_new_service::<HttpOutbound<T>, http::Request<http::BoxBody>>()
                .push(NewHttpGateway::layer(identity::LocalId(
                    self.inbound.identity().name().clone(),
                )))
                .check_new::<HttpOutbound<T>>()
                .check_new_service::<HttpOutbound<T>, http::Request<http::BoxBody>>()
                // .push_on_service(svc::LoadShed::layer())
                // .lift_new()
                // .push(svc::NewOneshotRoute::layer_via(
                //     |(_permit, t): &(_, Http<T>)| ByRequestVersion(t.clone()),
                // ))
                .push_filter(
                    |(_, parent): (_, Http<T>)| -> Result<_, GatewayDomainInvalid> {
                        svc::Param::<Option<profiles::Receiver>>::param(&parent)
                            .map(|profile| HttpOutbound { profile, parent })
                            .ok_or(GatewayDomainInvalid)
                    },
                )
                .push(self.inbound.authorize_http())
                .check_new_service::<Http<T>, http::Request<http::BoxBody>>()
                .into_inner();

            self.inbound
                .clone()
                .with_stack(http)
                // - May write access logs.
                // - Handle HTTP downgrading, inbound-policy errors.
                // - XXX Set an identity header -- this should probably not be done
                //   in the gateway, though the value will be stripped by meshed
                //   servers.
                // - Initializes tracing.
                .push_http_server() // Teminate HTTP connections.
                .push_tcp_http_server()
                .check_new_service::<Http<T>, I>()
                .into_stack()
                .check_new_service::<Http<T>, I>()
        };

        let tcp_opaque = svc::stack(opaque)
            .check_new_service::<Opaque<T>, I>()
            .push_map_target(|(_permit, opaque): (_, Opaque<T>)| opaque)
            .push(self.inbound.authorize_tcp())
            .check_new_service::<Opaque<T>, I>();

        let protocol = tcp_http
            .push_switch(
                |parent: outbound::Discovery<T>| -> Result<_, Error> {
                    if let Some(proto) = (*parent).param() {
                        let version = match proto {
                            SessionProtocol::Http1 => http::Version::Http1,
                            SessionProtocol::Http2 => http::Version::H2,
                        };
                        return Ok(svc::Either::A(Http { parent, version }));
                    }

                    Ok(svc::Either::B(Opaque(parent)))
                },
                tcp_opaque.into_inner(),
            )
            .check_new_service::<outbound::Discovery<T>, I>();

        let discover = self
            .outbound
            .with_stack(protocol.into_inner())
            .push_discover(profiles)
            .check_new_service::<T, I>();

        discover
            .push_on_service(svc::BoxService::layer())
            .push(svc::ArcNewService::layer())
            .check_new_service::<T, I>()
    }
}

// === impl Opaque ===

impl<T> std::ops::Deref for Opaque<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl<T> svc::Param<inbound::policy::AllowPolicy> for Opaque<T>
where
    T: svc::Param<inbound::policy::AllowPolicy>,
{
    fn param(&self) -> inbound::policy::AllowPolicy {
        (*self.0).param()
    }
}

impl<T> svc::Param<Remote<ClientAddr>> for Opaque<T>
where
    T: svc::Param<Remote<ClientAddr>>,
{
    fn param(&self) -> Remote<ClientAddr> {
        (*self.0).param()
    }
}

impl<T> svc::Param<tls::ConditionalServerTls> for Opaque<T>
where
    T: svc::Param<tls::ConditionalServerTls>,
{
    fn param(&self) -> tls::ConditionalServerTls {
        (*self.0).param()
    }
}

impl<T> svc::Param<tls::ClientId> for Opaque<T>
where
    T: svc::Param<tls::ClientId>,
{
    fn param(&self) -> tls::ClientId {
        (*self.0).param()
    }
}

// === impl Http ===

impl<T> std::ops::Deref for Http<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &*self.parent
    }
}

impl<T> Param<http::Version> for Http<T> {
    fn param(&self) -> http::Version {
        self.version
    }
}

impl<T> Param<Option<profiles::Receiver>> for Http<T> {
    fn param(&self) -> Option<profiles::Receiver> {
        self.parent.param()
    }
}

impl<T> svc::Param<inbound::policy::AllowPolicy> for Http<T>
where
    T: svc::Param<inbound::policy::AllowPolicy>,
{
    fn param(&self) -> inbound::policy::AllowPolicy {
        (*self.parent).param()
    }
}

impl<T> svc::Param<Remote<ClientAddr>> for Http<T>
where
    T: svc::Param<Remote<ClientAddr>>,
{
    fn param(&self) -> Remote<ClientAddr> {
        (*self.parent).param()
    }
}

impl<T> svc::Param<tls::ConditionalServerTls> for Http<T>
where
    T: svc::Param<tls::ConditionalServerTls>,
{
    fn param(&self) -> tls::ConditionalServerTls {
        (*self.parent).param()
    }
}

impl<T> svc::Param<tls::ClientId> for Http<T>
where
    T: svc::Param<tls::ClientId>,
{
    fn param(&self) -> tls::ClientId {
        (*self.parent).param()
    }
}

// === impl Http ===

impl<T> std::ops::Deref for HttpOutbound<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &*self.parent
    }
}

/*
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
    type Key = Http<profiles::LogicalAddr>;
    type Error = Error;

    fn select(&self, req: &http::Request<B>) -> Result<Self::Key, Error> {
        if let Some(profile) = self.0.profile.clone() {
            if let Some(profiles::LogicalAddr(addr)) = profile.logical_addr() {
                return Http {
                    version: req.version(),
                    target: outbound::http::logical::Target::Route(addr, profile),
                };
            }
        }

        Err(GatewayDomainInvalid.into())
    }
}

// === impl OutboundHttp ===

impl Param<http::Version> for OutboundHttp {
    fn param(&self) -> http::Version {
        self.version
    }
}

impl svc::Param<Remote<ServerAddr>> for OutboundHttp {
    fn param(&self) -> Remote<ServerAddr> {
        todo!("not this")
    }
}

impl svc::Param<http::normalize_uri::DefaultAuthority> for OutboundHttp {
    fn param(&self) -> http::normalize_uri::DefaultAuthority {
        todo!("not this")
    }
}

impl svc::Param<Option<profiles::LogicalAddr>> for OutboundHttp {
    fn param(&self) -> Option<profiles::LogicalAddr> {
        self.profile.as_ref()?.logical_addr()
    }
}

impl svc::Param<Option<profiles::Receiver>> for OutboundHttp {
    fn param(&self) -> Option<profiles::Receiver> {
        self.profile.clone()
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

impl svc::Param<Remote<ServerAddr>> for Opaque {
    fn param(&self) -> Remote<ServerAddr> {
        Remote(ServerAddr(self.client.local_addr.into()))
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

impl svc::Param<Option<profiles::LogicalAddr>> for Opaque {
    fn param(&self) -> Option<profiles::LogicalAddr> {
        self.profile.logical_addr()
    }
}

impl svc::Param<Option<profiles::Receiver>> for Opaque {
    fn param(&self) -> Option<profiles::Receiver> {
        Some(self.profile.clone())
    }
}

*/
