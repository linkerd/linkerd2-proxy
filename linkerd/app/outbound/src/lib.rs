//! Configures and runs the outbound proxy.
//!
//! The outound proxy is responsible for routing traffic from the local
//! application to external network endpoints.

#![deny(warnings, rust_2018_idioms)]

mod discover;
pub mod http;
mod ingress;
mod resolve;
pub mod target;
pub mod tcp;
#[cfg(test)]
pub(crate) mod test_util;

use linkerd_app_core::{
    config::ProxyConfig,
    io, metrics, profiles,
    proxy::{api_resolve::Metadata, core::Resolve},
    svc, tls,
    transport::listen,
    Addr, AddrMatch, Error, ProxyRuntime,
};
use std::{collections::HashMap, net::SocketAddr, time::Duration};

const EWMA_DEFAULT_RTT: Duration = Duration::from_millis(30);
const EWMA_DECAY: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct Config {
    pub proxy: ProxyConfig,
    pub allow_discovery: AddrMatch,
}

#[derive(Clone, Debug)]
pub struct Outbound<S> {
    config: Config,
    runtime: ProxyRuntime,
    stack: svc::Stack<S>,
}

impl Outbound<()> {
    pub fn new(config: Config, runtime: ProxyRuntime) -> Self {
        Self {
            config,
            runtime,
            stack: svc::stack(()),
        }
    }

    pub fn with_stack<S>(self, stack: S) -> Outbound<S> {
        Outbound {
            config: self.config,
            runtime: self.runtime,
            stack: svc::stack(stack),
        }
    }
}

impl<S> Outbound<S> {
    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn runtime(&self) -> &ProxyRuntime {
        &self.runtime
    }

    pub fn into_stack(self) -> svc::Stack<S> {
        self.stack
    }

    pub fn into_inner(self) -> S {
        self.stack.into_inner()
    }

    pub fn push<L: svc::Layer<S>>(self, layer: L) -> Outbound<L::Service> {
        Outbound {
            config: self.config,
            runtime: self.runtime,
            stack: self.stack.push(layer),
        }
    }

    pub fn into_server<R, P, I>(
        self,
        resolve: R,
        profiles: P,
    ) -> impl svc::NewService<
        listen::Addrs,
        Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send>,
    >
    where
        S: svc::Service<http::Endpoint, Error = io::Error>
            + svc::Service<tcp::Endpoint, Error = io::Error>,
        S: Clone + Send + Sync + Unpin + 'static,
        <S as svc::Service<http::Endpoint>>::Response: tls::HasNegotiatedProtocol,
        <S as svc::Service<http::Endpoint>>::Response:
            tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin,
        <S as svc::Service<http::Endpoint>>::Future: Send + Unpin,
        <S as svc::Service<tcp::Endpoint>>::Response: tls::HasNegotiatedProtocol,
        <S as svc::Service<tcp::Endpoint>>::Response:
            tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin,
        <S as svc::Service<tcp::Endpoint>>::Future: Send,
        R: Resolve<Addr, Endpoint = Metadata, Error = Error> + Clone + Send + 'static,
        R::Resolution: Send,
        R::Future: Send + Unpin,
        P: profiles::GetProfile<SocketAddr> + Clone + Send + 'static,
        P::Future: Send,
        P::Error: Send,
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + std::fmt::Debug + Send + Unpin + 'static,
    {
        let http = self
            .clone()
            .push_tcp_endpoint()
            .push_http_endpoint()
            .push_http_logical(resolve.clone())
            .push_http_server()
            .into_inner();

        self.push_tcp_endpoint()
            .push_tcp_balance(resolve)
            .push_tcp_logical()
            .push_detect_http(http)
            .push_discover(profiles)
            .into_inner()
    }
}

fn stack_labels(proto: &'static str, name: &'static str) -> metrics::StackLabels {
    metrics::StackLabels::outbound(proto, name)
}

pub fn trace_labels() -> HashMap<String, String> {
    let mut l = HashMap::new();
    l.insert("direction".to_string(), "outbound".to_string());
    l
}
