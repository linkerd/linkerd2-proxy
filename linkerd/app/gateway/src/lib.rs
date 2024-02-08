#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![allow(opaque_hidden_inferred_bound)]
#![forbid(unsafe_code)]

use linkerd_app_core::{
    io,
    metrics::prom,
    profiles,
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
    },
    svc, tls,
    transport::addrs::*,
    transport_header::SessionProtocol,
    Error, NameMatch,
};
use linkerd_app_inbound::{self as inbound, GatewayAddr, Inbound};
use linkerd_app_outbound::{self as outbound, Outbound};
use std::fmt::Debug;

mod discover;
mod http;
mod opaq;
mod server;

#[derive(Clone, Debug, Default)]
pub struct Config {
    pub allow_discovery: NameMatch,
}

/// Gateway stack builder utility.
#[derive(Clone)]
pub struct Gateway {
    config: Config,
    inbound: Inbound<()>,
    outbound: Outbound<()>,
}

impl Gateway {
    pub fn new(config: Config, inbound: Inbound<()>, outbound: Outbound<()>) -> Self {
        Self {
            config,
            inbound,
            outbound,
        }
    }

    /// Builds a gateway to the outbound stack, to be passed to the inbound
    /// stack.
    pub fn stack<T, I, R>(
        self,
        registry: &mut prom::Registry,
        resolve: R,
        profiles: impl profiles::GetProfile<Error = Error>,
        policies: impl outbound::policy::GetPolicy,
    ) -> svc::Stack<svc::ArcNewTcp<T, I>>
    where
        // Target describing an inbound gateway connection.
        T: svc::Param<GatewayAddr>,
        T: svc::Param<OrigDstAddr>,
        T: svc::Param<Remote<ClientAddr>>,
        T: svc::Param<tls::ConditionalServerTls>,
        T: svc::Param<tls::ClientId>,
        T: svc::Param<inbound::policy::AllowPolicy>,
        T: svc::Param<Option<SessionProtocol>>,
        T: Clone + Send + Sync + Unpin + 'static,
        // Server-side socket
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr,
        I: Debug + Send + Sync + Unpin + 'static,
        // Endpoint resolution.
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>,
        R::Resolution: Unpin,
    {
        let opaq = {
            let registry = registry.sub_registry_with_prefix("tcp");
            let resolve = resolve.clone();
            let opaq = self
                .outbound
                .to_tcp_connect()
                .push_opaq_cached(registry, resolve);
            self.opaq(opaq.into_inner()).into_inner()
        };

        let http = {
            let (http_metrics, http_server_metrics) = {
                let reg = registry.sub_registry_with_prefix("http");
                (
                    outbound::http::HttpMetrics::register(reg),
                    inbound::http::ServerMetricFamilies::register(reg),
                )
            };
            let grpc_metrics =
                outbound::http::GrpcMetrics::register(registry.sub_registry_with_prefix("grpc"));
            let http = self
                .outbound
                .to_tcp_connect()
                .push_tcp_endpoint()
                .push_http_tcp_client();
            let http = self.http(http_metrics, grpc_metrics, http.into_inner(), resolve);
            self.inbound
                .clone()
                .with_stack(http.into_inner())
                .push_http_tcp_server(http_server_metrics)
                .into_inner()
        };

        self.server(profiles, policies, opaq, http)
    }
}
