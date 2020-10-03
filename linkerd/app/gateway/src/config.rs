use super::make::{MakeGateway, ProfileAddr};
use indexmap::IndexSet;
use linkerd2_app_core::{dns, profiles, proxy::http, transport::tls, Error};
use linkerd2_app_inbound::endpoint as inbound;
use linkerd2_app_outbound::endpoint as outbound;
use std::net::SocketAddr;

#[derive(Clone, Debug, Default)]
pub struct Config {
    pub suffixes: IndexSet<dns::Suffix>,
}

impl Config {
    pub fn build<R, O, S>(
        self,
        resolve: R,
        outbound: O,
        default_addr: SocketAddr,
        local_id: tls::PeerIdentity,
    ) -> impl tower::Service<
        inbound::Target,
        Error = Error,
        Future = impl Send + 'static,
        Response = impl tower::Service<
            http::Request<http::boxed::Payload>,
            Response = http::Response<http::boxed::Payload>,
            Error = impl Into<Error>,
            Future = impl Send,
        > + Send
                       + 'static,
    > + Clone
           + Send
    where
        R: profiles::GetProfile<ProfileAddr> + Send + Clone,
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
        MakeGateway::new(
            resolve,
            outbound,
            default_addr,
            local_id,
            self.suffixes.clone(),
        )
    }
}
