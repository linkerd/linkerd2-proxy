use super::make::MakeGateway;
use linkerd_app_core::{
    discovery_rejected, profiles, proxy::http, svc, tls, Error, NameAddr, NameMatch,
};
use linkerd_app_inbound as inbound;
use linkerd_app_outbound as outbound;
use tracing::debug_span;

#[derive(Clone, Debug, Default)]
pub struct Config {
    pub allow_discovery: NameMatch,
}

#[derive(Clone, Debug)]
struct Allow(NameMatch);

pub fn stack<O, P, S>(
    Config { allow_discovery }: Config,
    outbound: O,
    profiles: P,
    local_id: Option<tls::LocalId>,
) -> impl svc::NewService<
    inbound::HttpGatewayTarget,
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
                Allow(allow_discovery),
            ))
            .instrument(
                |g: &inbound::HttpGatewayTarget| debug_span!("gateway", target = %g.target, v = %g.version),
            )
            .into_inner()
}

impl svc::stack::Predicate<inbound::HttpGatewayTarget> for Allow {
    type Request = NameAddr;

    fn check(&mut self, t: inbound::HttpGatewayTarget) -> Result<NameAddr, Error> {
        // The service name needs to exist in the configured set of suffixes.
        if self.0.matches(t.target.name()) {
            Ok(t.target)
        } else {
            Err(discovery_rejected().into())
        }
    }
}
