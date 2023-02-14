use self::gateway::NewHttpGateway;
use super::OutboundHttp;
use linkerd_app_core::{
    identity, io,
    proxy::{api_resolve::Metadata, core::Resolve, http},
    svc,
    transport::{ClientAddr, Local},
    Error, Infallible,
};
use linkerd_app_outbound::{self as outbound, Outbound};

mod gateway;
#[cfg(test)]
mod tests;

pub use linkerd_app_core::proxy::http::*;

/// Builds an outbound HTTP stack.
///
/// A gateway-specififc module is inserted to requests from looping through
/// gateways. Discovery errors are lifted into the HTTP stack so that individual
/// requests are failed with an HTTP-level error repsonse.
pub(super) fn stack<O, R>(
    local_id: identity::LocalId,
    outbound: Outbound<O>,
    resolve: R,
) -> svc::ArcNewService<
    OutboundHttp,
    impl svc::Service<
        http::Request<http::BoxBody>,
        Response = http::Response<http::BoxBody>,
        Error = Error,
        Future = impl Send,
    >,
>
where
    O: Clone + Send + Sync + Unpin + 'static,
    O: svc::MakeConnection<outbound::tcp::Connect, Metadata = Local<ClientAddr>, Error = io::Error>,
    O::Connection: Send + Unpin,
    O::Future: Send + Unpin + 'static,
    R: Resolve<outbound::http::Concrete, Endpoint = Metadata, Error = Error>,
{
    let endpoint = outbound.push_tcp_endpoint().push_http_endpoint();
    endpoint
        .clone()
        .push_http_concrete(resolve)
        .push_http_logical()
        .into_stack()
        .push_switch(Ok::<_, Infallible>, endpoint.into_stack())
        .push(NewHttpGateway::layer(local_id))
        .push(svc::ArcNewService::layer())
        .check_new::<OutboundHttp>()
        .into_inner()
}
