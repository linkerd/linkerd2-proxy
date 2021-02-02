use super::gateway::Gateway;
use linkerd_app_core::{profiles, svc, tls, NameAddr};
use linkerd_app_inbound as inbound;
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

pub(crate) type Target = (Option<profiles::Receiver>, inbound::HttpGatewayTarget);

impl<O> svc::NewService<Target> for MakeGateway<O>
where
    O: svc::NewService<outbound::http::Logical> + Send + Clone + 'static,
{
    type Service = Gateway<O::Service>;

    fn new_service(&mut self, (profile, target): Target) -> Self::Service {
        let inbound::HttpGatewayTarget { target, version } = target;

        let local_id = match self.local_id.clone() {
            Some(id) => id,
            None => return Gateway::NoIdentity,
        };

        let dst = match profile.as_ref().and_then(|p| p.borrow().name.clone()) {
            Some(name) => NameAddr::from((name, target.port())),
            None => return Gateway::BadDomain(target.name().clone()),
        };

        // Create an outbound target using the resolved name and an address
        // including the original port. We don't know the IP of the target, so
        // we use an unroutable one.
        debug!("Creating outbound service");
        let svc = self.outbound.new_service(outbound::http::Logical {
            profile,
            protocol: version,
            orig_dst: ([0, 0, 0, 0], dst.port()).into(),
        });

        Gateway::new(svc, target, local_id)
    }
}
