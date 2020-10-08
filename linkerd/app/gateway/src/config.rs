use super::make::MakeGateway;
use linkerd2_app_core::{
    discovery_rejected, profiles, proxy::http, svc, transport::tls, Error, NameAddr, NameMatch,
};
use linkerd2_app_inbound::endpoint as inbound;
use linkerd2_app_outbound::endpoint as outbound;
use std::net::SocketAddr;

#[derive(Clone, Debug, Default)]
pub struct Config {
    pub allow_discovery: NameMatch,
}

#[derive(Clone, Debug)]
struct Allow(NameMatch);

impl Config {
    pub fn build<O, P, S>(
        self,
        outbound: O,
        profiles: P,
        default_addr: SocketAddr,
        local_id: tls::PeerIdentity,
    ) -> impl svc::NewService<
        inbound::Target,
        Service = impl tower::Service<
            http::Request<http::boxed::Payload>,
            Response = http::Response<http::boxed::Payload>,
            Error = impl Into<Error>,
            Future = impl Send,
        > + Send
                      + 'static,
    > + Clone
           + Send
    where
        P: profiles::GetProfile<NameAddr> + Clone + Send + 'static,
        P::Future: Send + 'static,
        P::Error: Send,
        O: svc::NewService<outbound::HttpLogical, Service = S> + Clone + Send + 'static,
        S: tower::Service<
                http::Request<http::boxed::Payload>,
                Response = http::Response<http::boxed::Payload>,
            > + Send
            + 'static,
        S::Error: Into<Error>,
        S::Future: Send + 'static,
    {
        svc::stack(MakeGateway::new(outbound, default_addr, local_id))
            .check_new_service::<super::make::Target, http::Request<http::boxed::Payload>>()
            .push(profiles::discover::layer(
                profiles,
                Allow(self.allow_discovery),
            ))
            .check_new_service::<inbound::Target, http::Request<http::boxed::Payload>>()
            .into_inner()
    }
}

impl svc::stack::FilterRequest<inbound::Target> for Allow {
    type Request = NameAddr;

    fn filter(&self, target: inbound::Target) -> Result<NameAddr, Error> {
        // Skip discovery when the client does not have an identity.
        if target.tls_client_id.is_some() {
            // Discovery needs to have resolved a service name.
            if let Some(addr) = target.dst.into_name_addr() {
                // The service name needs to exist in the configured set of suffixes.
                if self.0.matches(addr.name()) {
                    return Ok(addr);
                }
            }
        }

        Err(discovery_rejected().into())
    }
}
