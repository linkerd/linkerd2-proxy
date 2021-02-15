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
    serve, svc,
    transport::listen,
    AddrMatch, Error, ProxyRuntime,
};
use std::{collections::HashMap, future::Future, net::SocketAddr, time::Duration};
use tracing::info;

const EWMA_DEFAULT_RTT: Duration = Duration::from_millis(30);
const EWMA_DECAY: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct Config {
    pub proxy: ProxyConfig,
    pub allow_discovery: AddrMatch,

    // In "ingress mode", we assume we are always routing HTTP requests and do
    // not perform per-target-address discovery. Non-HTTP connections are
    // forwarded without discovery/routing/mTLS.
    pub ingress_mode: bool,
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
}

impl<C> Outbound<C> {
    pub fn into_server<R, P, I>(
        self,
        resolve: R,
        profiles: P,
    ) -> impl svc::NewService<
        listen::Addrs,
        Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send>,
    >
    where
        C: svc::Connect + Send + Sync + Unpin + 'static,
        C::Io: 'static,
        C::Future: Unpin + 'static,
        R: Resolve<http::Concrete, Endpoint = Metadata, Error = Error>,
        <R as Resolve<http::Concrete>>::Resolution: Send,
        <R as Resolve<http::Concrete>>::Future: Send + Unpin,
        R: Resolve<tcp::Concrete, Endpoint = Metadata, Error = Error>,
        <R as Resolve<tcp::Concrete>>::Resolution: Send,
        <R as Resolve<tcp::Concrete>>::Future: Send + Unpin,
        R: Clone + Send + 'static,
        P: profiles::GetProfile<profiles::LogicalAddr> + Clone + Send + 'static,
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
            .push_tcp_logical(resolve)
            .push_detect_http(http)
            .push_discover(profiles)
            .into_inner()
    }

    pub fn serve<P, R>(self, profiles: P, resolve: R) -> (SocketAddr, impl Future<Output = ()>)
    where
        C: svc::Connect + Send + Sync + Unpin + 'static,
        C::Io: 'static,
        C::Future: Unpin + 'static,
        R: Resolve<http::Concrete, Endpoint = Metadata, Error = Error>,
        <R as Resolve<http::Concrete>>::Resolution: Send,
        <R as Resolve<http::Concrete>>::Future: Send + Unpin,
        R: Resolve<tcp::Concrete, Endpoint = Metadata, Error = Error>,
        <R as Resolve<tcp::Concrete>>::Resolution: Send,
        <R as Resolve<tcp::Concrete>>::Future: Send + Unpin,
        R: Clone + Send + Sync + Unpin + 'static,
        P: profiles::GetProfile<profiles::LogicalAddr> + Clone + Send + Sync + Unpin + 'static,
        P::Future: Send,
        P::Error: Send,
    {
        let (listen_addr, listen) = self
            .config
            .proxy
            .server
            .bind
            .bind()
            .expect("Failed to bind outbound listener");

        let serve = async move {
            let shutdown = self.runtime.drain.clone().signaled();
            if self.config.ingress_mode {
                info!("Outbound routing in ingress-mode");
                let tcp = self
                    .clone()
                    .push_tcp_endpoint()
                    .push_tcp_forward()
                    .into_inner();
                let ingress = self
                    .push_tcp_endpoint()
                    .push_http_endpoint()
                    .push_http_logical(resolve)
                    .into_ingress(tcp, profiles);
                serve::serve(listen, ingress, shutdown)
                    .await
                    .expect("Outbound server failed");
            } else {
                serve::serve(listen, self.into_server(resolve, profiles), shutdown)
                    .await
                    .expect("Outbound server failed");
            }
        };

        (listen_addr, serve)
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
