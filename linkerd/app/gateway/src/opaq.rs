use super::{server::Opaq, Gateway};
use inbound::{GatewayAddr, GatewayDomainInvalid};
use linkerd_app_core::{io, profiles, svc, tls, transport::addrs::*, Error};
use linkerd_app_inbound as inbound;
use linkerd_app_outbound as outbound;

pub type Target = outbound::opaq::Target;

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
                    let profile = svc::Param::<Option<profiles::Receiver>>::param(&*opaq)
                        .ok_or(GatewayDomainInvalid)?;
                    if let Some(profiles::LogicalAddr(addr)) = profile.logical_addr() {
                        Ok(outbound::opaq::Target::Route(addr, profile))
                    } else if let Some((addr, metadata)) = profile.endpoint() {
                        Ok(outbound::opaq::Target::Forward(
                            Remote(ServerAddr(addr)),
                            metadata,
                        ))
                    } else {
                        Err(GatewayDomainInvalid)
                    }
                },
            )
            // Authorize connections to the gateway.
            .push(self.inbound.authorize_tcp())
            .push_on_service(svc::BoxService::layer())
            .push(svc::ArcNewService::layer())
    }
}
