use super::{server::Opaq, Gateway};
use inbound::{GatewayAddr, GatewayDomainInvalid};
use linkerd_app_core::{io, profiles, svc, tls, transport::addrs::*, Error};
use linkerd_app_inbound as inbound;
use linkerd_app_outbound as outbound;
use tokio::sync::watch;

#[derive(Clone, Debug)]
pub struct Target {
    addr: GatewayAddr,
    routes: watch::Receiver<outbound::opaq::Routes>,
}

impl Gateway {
    /// Wrap the provided outbound opaque stack with inbound authorization and
    /// gateway request routing.
    pub fn opaq<T, I, N, NSvc>(&self, inner: N) -> svc::Stack<svc::ArcNewTcp<Opaq<T>, I>>
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
        N: svc::NewService<Target, Service = NSvc> + Clone + Send + Sync + Unpin + 'static,
        NSvc: svc::Service<I, Response = (), Error = Error>,
        NSvc: Send + Unpin + 'static,
        NSvc::Future: Send + 'static,
    {
        svc::stack(inner)
            // Only permit gateway traffic to endpoints for which we have
            // discovery information.
            .push_filter(
                |(_, opaq): (_, Opaq<T>)| -> Result<_, GatewayDomainInvalid> {
                    // Fail connections were not resolved.
                    Target::try_from(opaq)
                },
            )
            // Authorize connections to the gateway.
            .push(self.inbound.authorize_tcp())
            .arc_new_tcp()
    }
}

impl<T> TryFrom<Opaq<T>> for Target
where
    T: svc::Param<GatewayAddr>,
{
    type Error = GatewayDomainInvalid;

    fn try_from(opaq: Opaq<T>) -> Result<Self, Self::Error> {
        let addr = opaq.param();
        let (routes, paddr) = outbound::opaq::routes_from_discovery(
            addr.0.clone().into(),
            svc::Param::param(&*opaq),
            svc::Param::param(&*opaq),
        );
        if paddr.is_none() {
            // The gateway address must be resolveable via the profile API.
            return Err(GatewayDomainInvalid);
        }

        Ok(Target { addr, routes })
    }
}

impl svc::Param<Option<profiles::LogicalAddr>> for Target {
    fn param(&self) -> Option<profiles::LogicalAddr> {
        let GatewayAddr(addr) = self.addr.clone();
        Some(profiles::LogicalAddr(addr))
    }
}

impl svc::Param<watch::Receiver<outbound::opaq::Routes>> for Target {
    fn param(&self) -> watch::Receiver<outbound::opaq::Routes> {
        self.routes.clone()
    }
}

impl PartialEq for Target {
    fn eq(&self, other: &Self) -> bool {
        self.addr == other.addr
    }
}

impl Eq for Target {}

impl std::hash::Hash for Target {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.addr.hash(state);
    }
}
