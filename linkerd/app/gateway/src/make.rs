use super::gateway::Gateway;
use linkerd_app_core::{profiles, svc, tls, Conditional, NameAddr};
use linkerd_app_inbound::target as inbound;
use linkerd_app_outbound as outbound;
use tracing::debug;

#[derive(Clone, Debug)]
pub(crate) struct MakeGateway<O> {
    outbound: O,
    local_id: Option<tls::LocalId>,
}

impl<O> MakeGateway<O> {
    pub fn new(outbound: O, local_id: Option<tls::LocalId>) -> Self {
        Self { outbound, local_id }
    }
}

pub(crate) type Target = (Option<profiles::Receiver>, inbound::Target);

impl<O> svc::NewService<Target> for MakeGateway<O>
where
    O: svc::NewService<outbound::http::Logical> + Send + Clone + 'static,
{
    type Service = Gateway<O::Service>;

    fn new_service(&mut self, (profile, target): Target) -> Self::Service {
        let inbound::Target {
            dst,
            tls,
            http_version,
            target_addr: _,
        } = target;

        let (source_id, local_id) = match (tls, self.local_id.clone()) {
            (
                Conditional::Some(tls::ServerTls::Terminated {
                    client_id: Some(src),
                }),
                Some(local),
            ) => (src, local),
            _ => return Gateway::NoIdentity,
        };

        let dst = match profile.as_ref().and_then(|p| p.borrow().name.clone()) {
            Some(name) => NameAddr::from((name, dst.port())),
            None => match dst.name_addr() {
                Some(n) => return Gateway::BadDomain(n.name().clone()),
                None => return Gateway::NoAuthority,
            },
        };

        // Create an outbound target using the resolved name and an address
        // including the original port. We don't know the IP of the target, so
        // we use an unroutable one.
        let target = outbound::http::Logical {
            profile,
            protocol: http_version,
            orig_dst: ([0, 0, 0, 0], dst.port()).into(),
        };
        debug!(?target, "Creating outbound service");
        let svc = self.outbound.new_service(target);

        Gateway::new(svc, dst, source_id, local_id)
    }
}
