//! Configures and runs the outbound proxy.
//!
//! The outound proxy is responsible for routing traffic from the local
//! application to external network endpoints.

#![deny(warnings, rust_2018_idioms)]

pub mod discover;
pub mod http;
pub mod ingress;
mod resolve;
pub mod server;
pub mod target;
pub mod tcp;
#[cfg(test)]
pub(crate) mod test_util;

use linkerd_app_core::{
    config::ProxyConfig,
    drain, io, metrics,
    opencensus::proto::trace::v1 as oc,
    profiles,
    proxy::{api_resolve::Metadata, core::resolve::Resolve},
    svc,
    transport::listen,
    Addr, AddrMatch, Error,
};
use std::net::SocketAddr;
use std::{collections::HashMap, time::Duration};
use tokio::sync::mpsc;

const EWMA_DEFAULT_RTT: Duration = Duration::from_millis(30);
const EWMA_DECAY: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct Config {
    pub proxy: ProxyConfig,
    pub allow_discovery: AddrMatch,
}

#[allow(clippy::too_many_arguments)]
pub fn stack<R, P, C, H, HSvc, I>(
    config: &Config,
    profiles: P,
    resolve: R,
    tcp_connect: C,
    http_router: H,
    metrics: metrics::Proxy,
    span_sink: Option<mpsc::Sender<oc::Span>>,
    drain: drain::Watch,
) -> impl svc::NewService<
    listen::Addrs,
    Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send>,
>
where
    I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + std::fmt::Debug + Send + Unpin + 'static,
    R: Resolve<Addr, Endpoint = Metadata, Error = Error> + Clone + Send + 'static,
    R::Resolution: Send,
    R::Future: Send + Unpin,
    C: svc::Service<tcp::Endpoint> + Clone + Send + 'static,
    C::Response: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin,
    C::Error: Into<Error>,
    C::Future: Send,
    H: svc::NewService<http::Logical, Service = HSvc> + Clone + Send + 'static,
    HSvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>
        + Send
        + 'static,
    HSvc::Error: Into<Error>,
    HSvc::Future: Send,
    P: profiles::GetProfile<SocketAddr> + Clone + Send + 'static,
    P::Future: Send,
    P::Error: Send,
{
    let tcp = tcp::balance::stack(&config.proxy, tcp_connect, resolve, &metrics, drain.clone());
    let http = http::server::stack(&config.proxy, &metrics, span_sink, http_router);
    let server = server::stack(&config.proxy, &metrics, drain, tcp, http);
    discover::stack(config, &metrics, profiles, server)
}

fn stack_labels(proto: &'static str, name: &'static str) -> metrics::StackLabels {
    metrics::StackLabels::outbound(proto, name)
}

pub fn trace_labels() -> HashMap<String, String> {
    let mut l = HashMap::new();
    l.insert("direction".to_string(), "outbound".to_string());
    l
}
