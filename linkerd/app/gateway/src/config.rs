use super::make::MakeGateway;
use indexmap::IndexSet;
use linkerd2_app_core::proxy::http;
use linkerd2_app_core::{dns, transport::tls, Error};
use linkerd2_app_inbound::endpoint as inbound;
use linkerd2_app_outbound::endpoint as outbound;
use std::net::IpAddr;

#[derive(Clone, Debug, Default)]
pub struct Config {
    pub suffixes: IndexSet<dns::Suffix>,
}

impl Config {
    pub fn build<R, O, S>(
        self,
        resolve: R,
        outbound: O,
        local_id: tls::PeerIdentity,
    ) -> impl Clone
           + Send
           + tower::Service<
        inbound::Target,
        Error = impl Into<Error>,
        Future = impl Send + 'static,
        Response = impl Send
                       + tower::Service<
            http::Request<http::boxed::Payload>,
            Response = http::Response<http::boxed::Payload>,
            Error = impl Into<Error>,
            Future = impl Send,
        > + 'static,
    >
    where
        R: tower::Service<dns::Name, Response = (dns::Name, IpAddr)> + Send + Clone,
        R::Error: Into<Error> + 'static,
        R::Future: Send + 'static,
        O: tower::Service<outbound::HttpLogical, Response = S> + Send + Clone + 'static,
        O::Error: Into<Error> + Send + 'static,
        O::Future: Send + 'static,
        S: Send
            + tower::Service<
                http::Request<http::boxed::Payload>,
                Response = http::Response<http::boxed::Payload>,
            > + 'static,
        S::Error: Into<Error> + Send + 'static,
        S::Future: Send,
    {
        MakeGateway::new(resolve, outbound, local_id, self.suffixes.clone())
    }
}
