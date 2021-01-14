use super::make::MakeGateway;
use linkerd_app_core::{
    discovery_rejected, identity, profiles, proxy::http, svc, Error, NameAddr, NameMatch,
};
use linkerd_app_inbound::endpoint as inbound;
use linkerd_app_outbound as outbound;
use tracing::debug_span;

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
        local_id: Option<identity::Name>,
    ) -> impl svc::NewService<
        inbound::Target,
        Service = impl tower::Service<
            http::Request<http::BoxBody>,
            Response = http::Response<http::BoxBody>,
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
        O: svc::NewService<outbound::http::Logical, Service = S> + Clone + Send + 'static,
        S: tower::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>
            + Send
            + 'static,
        S::Error: Into<Error>,
        S::Future: Send + 'static,
    {
        svc::stack(MakeGateway::new(outbound, local_id))
            .check_new_service::<super::make::Target, http::Request<http::BoxBody>>()
            .push(profiles::discover::layer(
                profiles,
                Allow(self.allow_discovery),
            ))
            .check_new_service::<inbound::Target, http::Request<http::BoxBody>>()
            .instrument(|_: &inbound::Target| debug_span!("gateway"))
            .into_inner()
    }
}

impl svc::stack::Predicate<inbound::Target> for Allow {
    type Request = NameAddr;

    fn check(&mut self, target: inbound::Target) -> Result<NameAddr, Error> {
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
