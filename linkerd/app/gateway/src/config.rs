use super::make::GatewayMake;
use indexmap::IndexSet;
use linkerd2_app_core::{dns, proxy::http, Error};
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
    ) -> impl Clone
           + Send
           + tower::Service<
        inbound::Target,
        Error = impl Into<Error>,
        Future = impl Send + 'static,
        Response = impl Send
                       + tower::Service<
            http::Request<http::glue::HttpBody>,
            Response = http::Response<http::boxed::Payload>,
            Error = impl Into<Error>,
            Future = impl Send,
        > + 'static,
    >
    where
        R: tower::Service<dns::Name, Response = (dns::Name, IpAddr)> + Send + Clone,
        R::Error: Into<Error> + 'static,
        R::Future: Send + 'static,
        O: tower::Service<outbound::Logical<outbound::HttpEndpoint>, Response = S>
            + Send
            + Clone
            + 'static,
        O::Error: Into<Error> + Send + 'static,
        O::Future: Send + 'static,
        S: Send
            + tower::Service<
                http::Request<http::glue::HttpBody>,
                Response = http::Response<http::boxed::Payload>,
            > + 'static,
        S::Error: Into<Error> + Send + 'static,
        S::Future: Send,
    {
        GatewayMake::new(resolve, outbound, self.suffixes.clone())
    }
}
