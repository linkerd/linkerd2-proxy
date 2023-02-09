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
    Addr, Error, NameMatch,
};
use linkerd_app_inbound::{self as inbound, Inbound};
use linkerd_app_outbound::{self as outbound, Outbound};
use outbound::opaque;
use std::{
    cmp::{Eq, PartialEq},
    fmt::Debug,
    hash::Hash,
};

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

// XXX These outbound types can't just reflect `T` blindly, because it will bust
// caching. We probably  need to extract everything we need from the inner `T`
// and move on with a new type.

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HttpOut<T = OrigDstAddr> {
    addr: GatewayAddr,
    target: outbound::http::logical::Target,
    version: http::Version,
    parent: T,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Opaq<T>(outbound::Discovery<T>);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct OpaqOut(outbound::opaque::logical::Target);

/// Implements `svc::router::SelectRoute` for outbound HTTP requests. An
/// `OutboundHttp` target is returned for each request using the request's HTTP
/// version.
///
/// The request's HTTP version may not match the target's original HTTP version
/// when proxies use HTTP/2 to transport HTTP/1 requests.
#[derive(Clone, Debug)]
struct ByRequestVersion<T>(HttpOut<T>);

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
        opaque: O,
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
        O: svc::NewService<OpaqOut, Service = OSvc>,
        O: Clone + Send + Sync + Unpin + 'static,
        OSvc: svc::Service<I, Response = (), Error = Error>,
        OSvc: Send + Unpin + 'static,
        OSvc::Future: Send + 'static,
        // HTTP outbound stack
        H: svc::NewService<HttpOut, Service = HSvc>,
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
            // XXX TODO THIS STACK PROBABLY NEEDS TO HANDLE CACHING LOAD BALANCERS,
            // etc.
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
                .push_switch(switch, self.opaque(opaque).into_inner())
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

    fn opaque<T, I, N, NSvc>(&self, inner: N) -> svc::Stack<svc::ArcNewTcp<Opaq<T>, I>>
    where
        // Target describing an inbound gateway connection.
        T: svc::Param<GatewayAddr>,
        T: svc::Param<OrigDstAddr>,
        T: svc::Param<Remote<ClientAddr>>,
        T: svc::Param<tls::ClientId>,
        T: svc::Param<tls::ConditionalServerTls>,
        T: svc::Param<inbound::policy::AllowPolicy>,
        T: Clone + Send + Sync + Unpin + 'static,
        // Server-side socket.
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + Send + Unpin + 'static,
        // Opaq outbound stack.
        N: svc::NewService<OpaqOut, Service = NSvc> + Clone + Send + Sync + Unpin + 'static,
        NSvc: svc::Service<I, Response = (), Error = Error>,
        NSvc: Send + Unpin + 'static,
        NSvc::Future: Send + 'static,
    {
        svc::stack(inner)
            .push_filter(
                |(_, Opaq(opaq)): (_, Opaq<T>)| -> Result<_, GatewayDomainInvalid> {
                    // Fail connections were not resolved.
                    let profile = svc::Param::<Option<profiles::Receiver>>::param(&opaq)
                        .ok_or(GatewayDomainInvalid)?;

                    let target = if let Some(profiles::LogicalAddr(addr)) = profile.logical_addr() {
                        outbound::opaque::logical::Target::Route(addr, profile)
                    } else if let Some((addr, metadata)) = profile.endpoint() {
                        outbound::opaque::logical::Target::Forward(
                            Remote(ServerAddr(addr)),
                            metadata,
                        )
                    } else {
                        return Err(GatewayDomainInvalid);
                    };

                    Ok(OpaqOut(target))
                },
            )
            .push(self.inbound.authorize_tcp())
            .push_on_service(svc::BoxService::layer())
            .push(svc::ArcNewService::layer())
    }

    fn http<T, I, N, NSvc>(&self, inner: N) -> svc::Stack<svc::ArcNewTcp<Http<T>, I>>
    where
        // Target describing an inbound gateway connection.
        T: svc::Param<GatewayAddr>,
        T: svc::Param<OrigDstAddr>,
        T: svc::Param<Remote<ClientAddr>>,
        T: svc::Param<tls::ConditionalServerTls>,
        T: svc::Param<tls::ClientId>,
        T: svc::Param<inbound::policy::AllowPolicy>,
        T: svc::Param<profiles::LookupAddr>,
        T: Clone + Send + Sync + Unpin + 'static,
        // Server-side socket.
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + Send + Unpin + 'static,
        // HTTP outbound stack.
        N: svc::NewService<HttpOut, Service = NSvc> + Clone + Send + Sync + Unpin + 'static,
        NSvc: svc::Service<
            http::Request<http::BoxBody>,
            Response = http::Response<http::BoxBody>,
            Error = Error,
        >,
        NSvc: Send + Unpin + 'static,
        NSvc::Future: Send + 'static,
    {
        let http = svc::stack(inner)
            .push_map_target(HttpOut::orphan)
            .push(NewHttpGateway::layer(identity::LocalId(
                self.inbound.identity().name().clone(),
            )))
            .push_on_service(svc::LoadShed::layer())
            .lift_new()
            .push(svc::NewOneshotRoute::layer_via(|t: &HttpOut<T>| {
                ByRequestVersion(t.clone())
            }))
            .push_filter(
                |(_, parent): (_, Http<T>)| -> Result<_, GatewayDomainInvalid> {
                    let profile = svc::Param::<Option<profiles::Receiver>>::param(&parent)
                        .ok_or(GatewayDomainInvalid)?;

                    let target = if let Some(profiles::LogicalAddr(addr)) = profile.logical_addr() {
                        outbound::http::logical::Target::Route(addr, profile)
                    } else if let Some((addr, metadata)) = profile.endpoint() {
                        outbound::http::logical::Target::Forward(Remote(ServerAddr(addr)), metadata)
                    } else {
                        return Err(GatewayDomainInvalid);
                    };

                    Ok(HttpOut {
                        target,
                        addr: (*parent).param(),
                        parent: (*parent).param(),
                        version: parent.version,
                    })
                },
            )
            .push(self.inbound.authorize_http())
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
            .push_http_server()
            // Teminate HTTP connections.
            .push_tcp_http_server()
            .into_stack()
    }
}

// === impl ByRequestVersion ===

impl<B, T: Clone> svc::router::SelectRoute<http::Request<B>> for ByRequestVersion<T> {
    type Key = HttpOut<T>;
    type Error = http::version::Unsupported;

    fn select(&self, req: &http::Request<B>) -> Result<Self::Key, Self::Error> {
        let mut t = self.0.clone();
        t.version = req.version().try_into()?;
        Ok(t)
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

// === impl OpaqOut ===

impl svc::Param<opaque::logical::Target> for OpaqOut {
    fn param(&self) -> opaque::logical::Target {
        self.0.clone()
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

impl<T> Param<inbound::policy::ServerLabel> for Http<T>
where
    T: svc::Param<inbound::policy::AllowPolicy>,
{
    fn param(&self) -> inbound::policy::ServerLabel {
        (**self).param().server_label()
    }
}

// === impl HttpOut ===

impl<T> HttpOut<T>
where
    T: svc::Param<GatewayAddr>,
    T: svc::Param<OrigDstAddr>,
{
    fn orphan(self) -> HttpOut {
        HttpOut {
            addr: self.parent.param(),
            target: self.target,
            version: self.version,
            parent: self.parent.param(),
        }
    }
}

impl<T> svc::Param<GatewayAddr> for HttpOut<T> {
    fn param(&self) -> GatewayAddr {
        self.addr.clone()
    }
}

impl<T> svc::Param<http::Version> for HttpOut<T> {
    fn param(&self) -> http::Version {
        self.version
    }
}

impl<T> svc::Param<OrigDstAddr> for HttpOut<T>
where
    T: svc::Param<OrigDstAddr>,
{
    fn param(&self) -> OrigDstAddr {
        self.parent.param()
    }
}

impl<T> svc::Param<tls::ClientId> for HttpOut<T>
where
    T: svc::Param<tls::ClientId>,
{
    fn param(&self) -> tls::ClientId {
        self.parent.param()
    }
}

impl<T> svc::Param<Remote<ServerAddr>> for HttpOut<T>
where
    T: svc::Param<OrigDstAddr>,
{
    fn param(&self) -> Remote<ServerAddr> {
        let OrigDstAddr(addr) = self.parent.param();
        Remote(ServerAddr(addr))
    }
}

impl<T> svc::Param<outbound::http::logical::Target> for HttpOut<T> {
    fn param(&self) -> outbound::http::logical::Target {
        self.target.clone()
    }
}
