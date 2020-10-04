use super::gateway::Gateway;
use linkerd2_app_core::{profiles, proxy::identity, svc, transport::tls};
use linkerd2_app_inbound::endpoint as inbound;
use linkerd2_app_outbound::endpoint as outbound;
use std::net::SocketAddr;

#[derive(Clone, Debug)]
pub(crate) struct MakeGateway<O> {
    outbound: O,
    default_addr: SocketAddr,
}

pub struct Target {
    pub profile: Option<profiles::Receiver>,
    pub inbound: inbound::Target,
    pub local_id: identity::Name,
}

impl<O> MakeGateway<O> {
    pub fn new(outbound: O, default_addr: SocketAddr) -> Self {
        Self {
            outbound,
            default_addr,
        }
    }
}

impl<T, O> svc::NewService<T> for MakeGateway<O>
where
    O: svc::NewService<outbound::HttpLogical> + Send + Clone + 'static,
    T: Into<Target>,
{
    type Service = Gateway<O::Service>;

    fn new_service(&mut self, t: T) -> Self::Service {
        let Target {
            profile,
            local_id,
            inbound:
                inbound::Target {
                    dst,
                    tls_client_id,
                    http_version: version,
                    ..
                },
        } = t.into();

        let source_identity = match tls_client_id {
            tls::Conditional::Some(id) => id,
            tls::Conditional::None(_) => return Gateway::NoIdentity,
        };

        let dst = match dst.into_name_addr() {
            Some(n) => n,
            None => return Gateway::NoAuthority,
        };

        if profile.is_none() {
            return Gateway::BadDomain(dst.name().clone());
        }

        // Create an outbound target using the resolved IP & name.
        let svc = self.outbound.new_service(outbound::HttpLogical {
            orig_dst: self.default_addr,
            version,
            profile,
        });

        Gateway::new(svc, source_identity, dst, local_id)
    }
}
