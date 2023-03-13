use crate::Gateway;
use futures::FutureExt;
use linkerd_app_core::{
    errors, io, profiles, proxy::http, svc, tls, transport::addrs::*,
    transport_header::SessionProtocol, Addr, Error,
};
use linkerd_app_inbound::{self as inbound, GatewayAddr, GatewayDomainInvalid};
use linkerd_app_outbound::{self as outbound};
use std::fmt::Debug;
use tokio::sync::watch;
use tracing::Instrument;

/// Target for HTTP stacks.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Http<T> {
    version: http::Version,
    parent: outbound::Discovery<T>,
}

/// Target for opaque stacks.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Opaq<T>(outbound::Discovery<T>);

impl Gateway {
    /// Builds a server stack that discovers configuration for the target's
    /// `GatewayAddr`. The target's `SessionProtocol` is used to determine which
    /// inner stack to use.
    pub fn server<T, I, O, H, OSvc, HSvc>(
        self,
        profiles: impl profiles::GetProfile<Error = Error>,
        policies: impl outbound::policy::GetPolicy,
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
        T: Clone + Send + Sync + Unpin + 'static,
        // Server-side socket
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr,
        I: Debug + Send + Sync + Unpin + 'static,
        // Opaq outbound stack
        O: svc::NewService<Opaq<T>, Service = OSvc>,
        O: Clone + Send + Sync + Unpin + 'static,
        OSvc: svc::Service<I, Response = (), Error = Error>,
        OSvc: Send + Unpin + 'static,
        OSvc::Future: Send + 'static,
        // HTTP outbound stack
        H: svc::NewService<Http<T>, Service = HSvc>,
        H: Clone + Send + Sync + Unpin + 'static,
        HSvc: svc::Service<I, Response = (), Error = Error>,
        HSvc: Send + Unpin + 'static,
        HSvc::Future: Send + 'static,
    {
        let protocol = svc::stack(http)
            .push_switch(
                |parent: outbound::Discovery<T>| -> Result<_, GatewayDomainInvalid> {
                    if let Some(proto) = (*parent).param() {
                        let version = match proto {
                            SessionProtocol::Http1 => http::Version::Http1,
                            SessionProtocol::Http2 => http::Version::H2,
                        };
                        return Ok(svc::Either::A(Http { parent, version }));
                    }

                    Ok(svc::Either::B(Opaq(parent)))
                },
                opaq,
            )
            .into_inner();

        let discover = {
            use futures::future;

            let allowlist = self.config.allow_discovery.clone();
            svc::mk(move |GatewayAddr(addr)| {
                tracing::debug!(%addr, "Discover");

                if !allowlist.matches(addr.name()) {
                    tracing::debug!(%addr, "Address not in gateway discovery allowlist");
                    return future::Either::Left(future::err(GatewayDomainInvalid.into()));
                }

                let profile = profiles
                    .clone()
                    .get_profile(profiles::LookupAddr(addr.clone().into()))
                    .instrument(tracing::debug_span!("profiles"));

                // TODO(eliza): we should probably also add the allowlist to
                // policy resolution...
                let policy = policies
                    .get_policy(addr.into())
                    .instrument(tracing::debug_span!("policy"));

                let f = future::join(profile, policy).map(|(profile, policy)| {
                    tracing::debug!("Discovered");
                    let policy = match policy {
                        Ok(policy) => policy,
                        // If the policy controller returned `NotFound`,
                        // indicating that it doesn't have a policy for this
                        // addr, then we can't gateway this address.
                        Err(e) if is_not_found(&e) => return Err(GatewayDomainInvalid.into()),
                        Err(e) => return Err(e),
                    };

                    Ok((profile?, Some(policy)))
                });
                future::Either::Right(f)
            })
        };

        self.outbound
            .with_stack(protocol)
            .push_discover(discover)
            .into_stack()
            .push_on_service(svc::BoxService::layer())
            .push(svc::ArcNewService::layer())
    }
}

// === impl Opaq ===

impl<T> std::ops::Deref for Opaq<T> {
    type Target = outbound::Discovery<T>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> svc::Param<inbound::policy::AllowPolicy> for Opaq<T>
where
    T: svc::Param<inbound::policy::AllowPolicy>,
{
    fn param(&self) -> inbound::policy::AllowPolicy {
        (***self).param()
    }
}

impl<T> svc::Param<Remote<ClientAddr>> for Opaq<T>
where
    T: svc::Param<Remote<ClientAddr>>,
{
    fn param(&self) -> Remote<ClientAddr> {
        (***self).param()
    }
}

impl<T> svc::Param<Remote<ServerAddr>> for Opaq<T>
where
    T: svc::Param<Remote<ServerAddr>>,
{
    fn param(&self) -> Remote<ServerAddr> {
        (***self).param()
    }
}

impl<T> svc::Param<tls::ConditionalServerTls> for Opaq<T>
where
    T: svc::Param<tls::ConditionalServerTls>,
{
    fn param(&self) -> tls::ConditionalServerTls {
        (***self).param()
    }
}

impl<T> svc::Param<tls::ClientId> for Opaq<T>
where
    T: svc::Param<tls::ClientId>,
{
    fn param(&self) -> tls::ClientId {
        (***self).param()
    }
}

impl<T> svc::Param<Option<profiles::Receiver>> for Opaq<T> {
    fn param(&self) -> Option<profiles::Receiver> {
        self.0.param()
    }
}

// === impl Http ===

impl<T> std::ops::Deref for Http<T> {
    type Target = outbound::Discovery<T>;

    fn deref(&self) -> &Self::Target {
        &self.parent
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

impl<T> svc::Param<Option<watch::Receiver<profiles::Profile>>> for Http<T> {
    fn param(&self) -> Option<watch::Receiver<profiles::Profile>> {
        self.parent.param()
    }
}

impl<T> svc::Param<inbound::policy::AllowPolicy> for Http<T>
where
    T: svc::Param<inbound::policy::AllowPolicy>,
{
    fn param(&self) -> inbound::policy::AllowPolicy {
        (***self).param()
    }
}

impl<T> svc::Param<GatewayAddr> for Http<T>
where
    T: svc::Param<GatewayAddr>,
{
    fn param(&self) -> GatewayAddr {
        (***self).param()
    }
}

impl<T> svc::Param<OrigDstAddr> for Http<T>
where
    T: svc::Param<OrigDstAddr>,
{
    fn param(&self) -> OrigDstAddr {
        (***self).param()
    }
}

impl<T> svc::Param<Remote<ClientAddr>> for Http<T>
where
    T: svc::Param<Remote<ClientAddr>>,
{
    fn param(&self) -> Remote<ClientAddr> {
        (***self).param()
    }
}

impl<T> svc::Param<Remote<ServerAddr>> for Http<T>
where
    T: svc::Param<Remote<ServerAddr>>,
{
    fn param(&self) -> Remote<ServerAddr> {
        (***self).param()
    }
}

impl<T> svc::Param<tls::ConditionalServerTls> for Http<T>
where
    T: svc::Param<tls::ConditionalServerTls>,
{
    fn param(&self) -> tls::ConditionalServerTls {
        (***self).param()
    }
}

impl<T> svc::Param<tls::ClientId> for Http<T>
where
    T: svc::Param<tls::ClientId>,
{
    fn param(&self) -> tls::ClientId {
        (***self).param()
    }
}

impl<T> svc::Param<http::normalize_uri::DefaultAuthority> for Http<T>
where
    T: svc::Param<GatewayAddr>,
{
    fn param(&self) -> http::normalize_uri::DefaultAuthority {
        let GatewayAddr(addr) = (***self).param();
        let authority = Addr::from(addr).to_http_authority();
        http::normalize_uri::DefaultAuthority(Some(authority))
    }
}

impl<T> svc::Param<inbound::policy::ServerLabel> for Http<T>
where
    T: svc::Param<inbound::policy::AllowPolicy>,
{
    fn param(&self) -> inbound::policy::ServerLabel {
        (***self).param().server_label()
    }
}

#[inline]
fn is_not_found(e: &Error) -> bool {
    errors::cause_ref::<tonic::Status>(e.as_ref())
        .map(|s| s.code() == tonic::Code::NotFound)
        .unwrap_or(false)
}
