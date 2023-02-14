#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use linkerd_app_core::{
    io, profiles, svc, tls, transport::addrs::*, transport_header::SessionProtocol, Addr, Error,
    NameMatch,
};
use linkerd_app_inbound::{self as inbound, GatewayAddr, GatewayDomainInvalid, Inbound};
use linkerd_app_outbound::{self as outbound, Outbound};
use std::fmt::Debug;

mod http;
mod opaq;
#[cfg(test)]
mod tests;

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

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Http<T> {
    version: http::Version,
    parent: outbound::Discovery<T>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Opaq<T>(outbound::Discovery<T>);

impl Gateway {
    pub fn new(config: Config, inbound: Inbound<()>, outbound: Outbound<()>) -> Self {
        Self {
            config,
            inbound,
            outbound,
        }
    }

    /// Builds a gateway between inbound and outbound proxy stacks.
    pub fn stack<T, I, P, O, H, OSvc, HSvc>(
        self,
        profiles: P,
        opaq: O,
        http: H,
    ) -> svc::Stack<svc::ArcNewTcp<T, I>>
    where
        // Target describing an inbound gateway connection.
        T: svc::Param<GatewayAddr>,
        T: svc::Param<OrigDstAddr>,
        T: svc::Param<Remote<ClientAddr>>,
        T: svc::Param<tls::ConditionalServerTls>,
        T: svc::Param<tls::ClientId>,
        T: svc::Param<inbound::policy::AllowPolicy>,
        T: svc::Param<Option<SessionProtocol>>,
        T: svc::Param<profiles::LookupAddr>,
        T: Clone + Send + Sync + Unpin + 'static,
        // Inbound socket
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + Debug + Send + Sync + Unpin + 'static,
        // Discovery
        P: profiles::GetProfile<Error = Error>,
        // Opaq outbound stack
        O: svc::NewService<opaq::Target, Service = OSvc>,
        O: Clone + Send + Sync + Unpin + 'static,
        OSvc: svc::Service<I, Response = (), Error = Error>,
        OSvc: Send + Unpin + 'static,
        OSvc::Future: Send + 'static,
        // HTTP outbound stack
        H: svc::NewService<http::Target, Service = HSvc>,
        H: Clone + Send + Sync + Unpin + 'static,
        HSvc: svc::Service<
            http::Request<http::BoxBody>,
            Response = http::Response<http::BoxBody>,
            Error = Error,
        >,
        HSvc: Send + Unpin + 'static,
        HSvc::Future: Send + 'static,
    {
        let protocol = {
            let switch = |parent: outbound::Discovery<T>| -> Result<_, GatewayDomainInvalid> {
                if let Some(proto) = (*parent).param() {
                    let version = match proto {
                        SessionProtocol::Http1 => http::Version::Http1,
                        SessionProtocol::Http2 => http::Version::H2,
                    };
                    return Ok(svc::Either::A(Http { parent, version }));
                }

                Ok(svc::Either::B(Opaq(parent)))
            };

            self.http(http)
                .push_switch(switch, self.opaq(opaq).into_inner())
                .into_inner()
        };

        // Override the outbound stack's discovery allow list to match the
        // gateway allow list.
        let discover = {
            let mut out = self.outbound.clone();
            out.config_mut().allow_discovery = self.config.allow_discovery.clone().into();
            out.with_stack(protocol)
                .push_discover(profiles)
                .into_stack()
        };

        discover
            .push_on_service(svc::BoxService::layer())
            .push(svc::ArcNewService::layer())
    }
}

// === impl Opaq ===

impl<T> std::ops::Deref for Opaq<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl<T> svc::Param<inbound::policy::AllowPolicy> for Opaq<T>
where
    T: svc::Param<inbound::policy::AllowPolicy>,
{
    fn param(&self) -> inbound::policy::AllowPolicy {
        (**self).param()
    }
}

impl<T> svc::Param<Remote<ClientAddr>> for Opaq<T>
where
    T: svc::Param<Remote<ClientAddr>>,
{
    fn param(&self) -> Remote<ClientAddr> {
        (**self).param()
    }
}

impl<T> svc::Param<Remote<ServerAddr>> for Opaq<T>
where
    T: svc::Param<Remote<ServerAddr>>,
{
    fn param(&self) -> Remote<ServerAddr> {
        (**self).param()
    }
}

impl<T> svc::Param<tls::ConditionalServerTls> for Opaq<T>
where
    T: svc::Param<tls::ConditionalServerTls>,
{
    fn param(&self) -> tls::ConditionalServerTls {
        (**self).param()
    }
}

impl<T> svc::Param<tls::ClientId> for Opaq<T>
where
    T: svc::Param<tls::ClientId>,
{
    fn param(&self) -> tls::ClientId {
        (**self).param()
    }
}

impl<T> svc::Param<Option<profiles::Receiver>> for Opaq<T> {
    fn param(&self) -> Option<profiles::Receiver> {
        self.0.param()
    }
}

// === impl Http ===

impl<T> std::ops::Deref for Http<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &*self.parent
    }
}

impl<T> svc::Param<http::Version> for Http<T> {
    fn param(&self) -> http::Version {
        self.version
    }
}

impl<T> svc::Param<Option<profiles::Receiver>> for Http<T> {
    fn param(&self) -> Option<profiles::Receiver> {
        self.parent.param()
    }
}

impl<T> svc::Param<inbound::policy::AllowPolicy> for Http<T>
where
    T: svc::Param<inbound::policy::AllowPolicy>,
{
    fn param(&self) -> inbound::policy::AllowPolicy {
        (**self).param()
    }
}

impl<T> svc::Param<OrigDstAddr> for Http<T>
where
    T: svc::Param<OrigDstAddr>,
{
    fn param(&self) -> OrigDstAddr {
        (**self).param()
    }
}

impl<T> svc::Param<Remote<ClientAddr>> for Http<T>
where
    T: svc::Param<Remote<ClientAddr>>,
{
    fn param(&self) -> Remote<ClientAddr> {
        (**self).param()
    }
}

impl<T> svc::Param<Remote<ServerAddr>> for Http<T>
where
    T: svc::Param<Remote<ServerAddr>>,
{
    fn param(&self) -> Remote<ServerAddr> {
        (**self).param()
    }
}

impl<T> svc::Param<tls::ConditionalServerTls> for Http<T>
where
    T: svc::Param<tls::ConditionalServerTls>,
{
    fn param(&self) -> tls::ConditionalServerTls {
        (**self).param()
    }
}

impl<T> svc::Param<tls::ClientId> for Http<T>
where
    T: svc::Param<tls::ClientId>,
{
    fn param(&self) -> tls::ClientId {
        (**self).param()
    }
}

impl<T> svc::Param<http::normalize_uri::DefaultAuthority> for Http<T>
where
    T: svc::Param<GatewayAddr>,
{
    fn param(&self) -> http::normalize_uri::DefaultAuthority {
        let GatewayAddr(addr) = (**self).param();
        let authority = Addr::from(addr).to_http_authority();
        http::normalize_uri::DefaultAuthority(Some(authority))
    }
}

impl<T> svc::Param<inbound::policy::ServerLabel> for Http<T>
where
    T: svc::Param<inbound::policy::AllowPolicy>,
{
    fn param(&self) -> inbound::policy::ServerLabel {
        (**self).param().server_label()
    }
}
