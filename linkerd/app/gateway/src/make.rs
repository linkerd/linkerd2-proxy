use super::gateway::Gateway;
use linkerd2_app_core::{profiles, svc, transport::tls, NameAddr};
use linkerd2_app_inbound::endpoint as inbound;
use linkerd2_app_outbound::endpoint as outbound;
use std::net::SocketAddr;

#[derive(Clone, Debug)]
pub(crate) struct MakeGateway<O> {
    outbound: O,
    default_addr: SocketAddr,
    local_id: tls::PeerIdentity,
}

impl<O> MakeGateway<O> {
    pub fn new(outbound: O, default_addr: SocketAddr, local_id: tls::PeerIdentity) -> Self {
        Self {
            outbound,
            default_addr,
            local_id,
        }
    }
}

pub(crate) type Target = (Option<profiles::Receiver>, inbound::Target);

impl<O> svc::NewService<Target> for MakeGateway<O>
where
    O: svc::NewService<outbound::HttpLogical> + Send + Clone + 'static,
{
    type Service = Gateway<O::Service>;

    fn new_service(&mut self, (profile, target): Target) -> Self::Service {
        let inbound::Target {
            dst,
            tls_client_id,
            http_version,
            socket_addr: _,
        } = target;

        let (source_id, local_id) = match (tls_client_id, self.local_id.clone()) {
            (tls::Conditional::Some(src), tls::Conditional::Some(local)) => (src, local),
            _ => return Gateway::NoIdentity,
        };

        let dst = match profile.as_ref().and_then(|p| p.borrow().name.clone()) {
            Some(name) => NameAddr::from((name, dst.port())),
            None => match dst.name_addr() {
                Some(n) => return Gateway::BadDomain(n.name().clone()),
                None => return Gateway::NoAuthority,
            },
        };

        // Create an outbound target using the resolved IP & name.
        let svc = self.outbound.new_service(outbound::HttpLogical {
            profile,
            orig_dst: self.default_addr,
            protocol: http_version,
        });

        Gateway::new(svc, dst, source_id, local_id)
    }
}
