use super::make::MakeGateway;
use indexmap::IndexSet;
use linkerd2_app_core::{dns, profiles, proxy::http, svc, transport::tls, Error};
use linkerd2_app_inbound::endpoint as inbound;
use linkerd2_app_outbound::endpoint as outbound;
use std::net::SocketAddr;

#[derive(Clone, Debug, Default)]
pub struct Config {
    pub suffixes: IndexSet<dns::Suffix>,
}

impl Config {
    pub fn build<O, P, S>(
        self,
        outbound: O,
        profils: P,
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
        P: profiles::GetProfile<inbound::Target>,
        O: svc::NewService<outbound::HttpLogical, Service = S> + Clone,
        S: tower::Service<
            http::Request<http::boxed::Payload>,
            Response = http::Response<http::boxed::Payload>,
        >,
    {
        svc::stack(MakeGateway::new(outbound, default_addr))
    }
}
