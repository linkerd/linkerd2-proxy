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
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
    },
    serve,
    svc::{self, stack::Param},
    tls,
    transport::{self, OrigDstAddr},
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

    pub fn into_server<R, P, A, I>(
        self,
        resolve: R,
        profiles: P,
        addrs: A,
    ) -> impl for<'a> svc::NewService<
        &'a I,
        Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send>,
    >
    where
        A: listen::GetAddrs<I>,
        A::Addrs: Param<Option<OrigDstAddr>>,
        S: Clone + Send + Sync + Unpin + 'static,
        S: svc::Service<tcp::Connect, Error = io::Error>,
        S::Response: tls::HasNegotiatedProtocol,
        S::Response: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin,
        S::Future: Send + Unpin,
        R: Clone + Send + 'static,
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>,
        R::Resolution: Send,
        R::Future: Send + Unpin,
        P: profiles::GetProfile<profiles::LookupAddr> + Clone + Send + 'static,
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
            .push_request_filter(transport::AddrsFilter(addrs))
            .into_inner()
    }
}

impl Outbound<()> {
    pub fn serve<P, R>(
        self,
        profiles: P,
        resolve: R,
        addrs: A,
    ) -> (SocketAddr, impl Future<Output = ()>)
    where
        // TODO(eliza): make `serve` generic over incoming conns (pass in a stream
        // of IOs?) rather than making this hardcoded to `TcpStream` here?
        A: listen::GetAddrs<TcpStream>,
        A::Addrs: Param<Option<OrigDstAddr>>,
        R: Clone + Send + Sync + Unpin + 'static,
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>,
        R::Resolution: Send,
        R::Future: Send + Unpin,
        P: profiles::GetProfile<profiles::LookupAddr> + Clone + Send + Sync + Unpin + 'static,
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
            if self.config.ingress_mode {
                info!("Outbound routing in ingress-mode");
                let tcp = self
                    .to_tcp_connect()
                    .push_tcp_endpoint()
                    .push_tcp_forward()
                    .into_inner();
                let http = self
                    .to_tcp_connect()
                    .push_tcp_endpoint()
                    .push_http_endpoint()
                    .push_http_logical(resolve)
                    .into_inner();
                let stack = self.to_ingress(profiles, tcp, http, addrs);
                let shutdown = self.runtime.drain.signaled();
                serve::serve(listen, stack, shutdown).await;
            } else {
                let stack = self.to_tcp_connect().into_server(resolve, profiles, addrs);
                let shutdown = self.runtime.drain.signaled();
                serve::serve(listen, stack, shutdown).await;
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
