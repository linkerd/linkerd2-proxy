//! Configures and runs the outbound proxy.
//!
//! The outound proxy is responsible for routing traffic from the local
//! application to external network endpoints.

#![deny(warnings, rust_2018_idioms)]

mod discover;
pub mod endpoint;
pub mod http;
mod ingress;
pub mod logical;
mod resolve;
pub mod tcp;
#[cfg(test)]
pub(crate) mod test_util;

use crate::http::SkipHttpDetection;
use linkerd_app_core::{
    config::{ProxyConfig, ServerConfig},
    io, metrics, profiles,
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
    },
    serve,
    svc::{self, stack::Param},
    tls,
    transport::{self, addrs::*, listen::Bind},
    AddrMatch, Conditional, Error, ProxyRuntime,
};
use std::{collections::HashMap, future::Future, time::Duration};
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

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct Accept<P> {
    pub orig_dst: OrigDstAddr,
    pub protocol: P,
}

// === impl Outbound ===

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

    pub fn into_server<T, R, P, I>(
        self,
        resolve: R,
        profiles: P,
    ) -> impl svc::NewService<
        T,
        Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send>,
    >
    where
        Self: Clone + 'static,
        T: Param<OrigDstAddr> + Param<Remote<ClientAddr>> + Clone + Send + Sync + 'static,
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
        let tcp_endpoint = self.clone().push_tcp_endpoint().into_inner();
        let http_endpoint = self.clone().push_tcp_endpoint().into_inner();

        Outbound::new(self.config, self.runtime).into_server_with(
            resolve,
            profiles,
            http_endpoint,
            tcp_endpoint,
        )
    }
}

impl Outbound<()> {
    /// Builds an outbound server stack with the provided HTTP and raw TCP
    /// endpoint stacks.
    pub(crate) fn into_server_with<T, R, P, I, H, N>(
        self,
        resolve: R,
        profiles: P,
        http_endpoint: H,
        tcp_endpoint: N,
    ) -> impl svc::NewService<
        T,
        Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send>,
    >
    where
        Self: Clone + 'static,
        T: Param<OrigDstAddr> + Param<Remote<ClientAddr>> + Clone + Send + Sync + 'static,
        R: Clone + Send + 'static,
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>,
        R::Resolution: Send,
        R::Future: Send + Unpin,
        P: profiles::GetProfile<profiles::LookupAddr> + Clone + Send + 'static,
        P::Future: Send,
        P::Error: Send,
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + std::fmt::Debug + Send + Unpin + 'static,
        H: svc::Service<http::Endpoint, Error = Error>,
        H: Unpin + Clone + Send + Sync + 'static,
        H::Response: io::AsyncRead + io::AsyncWrite + Send + Unpin + 'static,
        H::Future: Unpin + Send + 'static,
        N: svc::Service<tcp::Endpoint, Error = Error>,
        N: Unpin + Clone + Send + 'static,
        N::Response: io::AsyncRead + io::AsyncWrite + Send + Unpin + 'static,
        N::Future: Send + 'static,
    {
        let tcp_endpoint = self.clone().with_stack(tcp_endpoint);
        let http_endpoint = self.with_stack(http_endpoint).push_http_endpoint();

        // HTTP per-endpoint stack used when a profile is not discovered.
        let http_no_profile = http_endpoint
            .clone()
            .push_into_endpoint()
            .push_http_server::<http::Accept, _>()
            .into_inner();

        let tcp_forward = tcp_endpoint.clone().push_tcp_forward();

        // HTTP and TCP per-endpoint stack used when a profile is not discovered.
        let no_profile = tcp_forward
            .clone()
            .push_into_endpoint::<(), tcp::Accept>()
            // If HTTP is detected, use the `http_endpoint` stack
            .push_detect_http(http_no_profile)
            .into_inner();

        // HTTP and TCP per-endpoint stack used when the service profile
        // has an endpoint override.
        let profile_endpoint = tcp_forward
            .push_detect_http::<endpoint::ProfileEndpoint, _, _, _, _, _, _>(
                http_endpoint
                    .clone()
                    .push_http_server::<http::Endpoint, _>()
                    .into_inner(),
            )
            .into_stack()
            .instrument(|ep: &endpoint::ProfileEndpoint| {
                let addr: Remote<ServerAddr> = ep.param();
                tracing::debug_span!("endpoint", %addr)
            })
            .check_new_service::<endpoint::ProfileEndpoint, _>()
            .into_inner();

        // HTTP stack for logical targets (with service profiles).
        let http_logical = http_endpoint
            .push_http_logical(resolve.clone())
            .push_http_server()
            .into_inner();

        tcp_endpoint
            .push_tcp_logical(resolve)
            // Try to detect HTTP and use the `http_logical` stack, skipping
            // detection if it's disabled by the service profile.
            .push_detect_http(http_logical)
            // If a service profile was not discovered, fall back to the
            // per-endpoint stack.
            .push_unwrap_logical(no_profile)
            // If a service profile was discovered, and it contains an
            // overridden endpoint, bypass the logical stack and forward
            // directly to that endpoint.
            .push_profile_endpoint(profile_endpoint)
            // Discover service profiles for each original dst address
            .push_discover(profiles)
            .into_inner()
    }

    pub fn serve<B, P, R>(
        self,
        bind: B,
        profiles: P,
        resolve: R,
    ) -> (Local<ServerAddr>, impl Future<Output = ()>)
    where
        B: Bind<ServerConfig>,
        B::Addrs: Param<Remote<ClientAddr>> + Param<OrigDstAddr>,
        R: Clone + Send + Sync + Unpin + 'static,
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>,
        R::Resolution: Send,
        R::Future: Send + Unpin,
        P: profiles::GetProfile<profiles::LookupAddr> + Clone + Send + Sync + Unpin + 'static,
        P::Future: Send,
        P::Error: Send,
    {
        let (listen_addr, listen) = bind
            .bind(&self.config.proxy.server)
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
                let stack = self.to_ingress(profiles, tcp, http);
                let shutdown = self.runtime.drain.signaled();
                serve::serve(listen, stack, shutdown).await;
            } else {
                let stack = self.to_tcp_connect().into_server(resolve, profiles);
                let shutdown = self.runtime.drain.signaled();
                serve::serve(listen, stack, shutdown).await;
            }
        };

        (listen_addr, serve)
    }
}

// === impl Accept ===

impl<P> Param<transport::labels::Key> for Accept<P> {
    fn param(&self) -> transport::labels::Key {
        const NO_TLS: tls::ConditionalServerTls = Conditional::None(tls::NoServerTls::Loopback);
        transport::labels::Key::accept(
            transport::labels::Direction::Out,
            NO_TLS,
            self.orig_dst.into(),
        )
    }
}

impl<P> Param<OrigDstAddr> for Accept<P> {
    fn param(&self) -> OrigDstAddr {
        self.orig_dst
    }
}

// When a profile is not discovered, always enable protocol detection.
impl Param<SkipHttpDetection> for Accept<()> {
    fn param(&self) -> SkipHttpDetection {
        SkipHttpDetection(false)
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
