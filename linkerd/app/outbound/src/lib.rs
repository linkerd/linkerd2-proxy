//! Configures and runs the outbound proxy.
//!
//! The outbound proxy is responsible for routing traffic from the local application to other hosts.

#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use futures::Stream;
use linkerd_app_core::{
    config::{ProxyConfig, QueueConfig},
    drain,
    http_tracing::OpenCensusSink,
    identity, io, profiles,
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
        tap,
    },
    serve,
    svc::{self, stack::Param},
    tls,
    transport::addrs::*,
    AddrMatch, Error, ProxyRuntime, Result,
};
use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    net::IpAddr,
    sync::Arc,
    time::Duration,
};

mod discover;
pub mod http;
mod ingress;
mod metrics;
pub mod opaq;
pub mod policy;
mod protocol;
mod sidecar;
pub mod tcp;
#[cfg(any(test, feature = "test-util"))]
pub mod test_util;

pub use self::{discover::Discovery, metrics::Metrics};

#[derive(Clone, Debug)]
pub struct Config {
    pub allow_discovery: AddrMatch,

    pub proxy: ProxyConfig,

    /// Configures the duration the proxy will retain idle stacks (with no
    /// active connections) for an outbound address. When an idle stack is
    /// dropped, all cached service discovery information is dropped.
    pub discovery_idle_timeout: Duration,

    /// Configures how connections are buffered *for each outbound address*.
    ///
    /// A buffer capacity of 100 means that 100 connections may be buffered for
    /// each IP:port to which an application attempts to connect.
    pub tcp_connection_queue: QueueConfig,

    /// Configures how HTTP requests are buffered *for each outbound address*.
    ///
    /// A buffer capacity of 100 means that 100 requests may be buffered for
    /// each IP:port to which an application has opened an outbound TCP connection.
    pub http_request_queue: QueueConfig,

    // In "ingress mode", we assume we are always routing HTTP requests and do
    // not perform per-target-address discovery. Non-HTTP connections are
    // forwarded without discovery/routing/mTLS.
    pub ingress_mode: bool,
    pub inbound_ips: Arc<HashSet<IpAddr>>,

    // Whether the proxy may include informational headers on HTTP responses.
    pub emit_headers: bool,
}

#[derive(Clone, Debug)]
pub struct Outbound<S> {
    config: Config,
    runtime: Runtime,
    stack: svc::Stack<S>,
}

#[derive(Clone, Debug)]
struct Runtime {
    metrics: Metrics,
    identity: identity::NewClient,
    tap: tap::Registry,
    span_sink: OpenCensusSink,
    drain: drain::Watch,
}

pub type ConnectMeta = tls::ConnectMeta<Local<ClientAddr>>;

// === impl Outbound ===

impl Outbound<()> {
    pub fn new(config: Config, runtime: ProxyRuntime) -> Self {
        let runtime = Runtime {
            metrics: Metrics::new(runtime.metrics),
            identity: runtime.identity.new_client(),
            tap: runtime.tap,
            span_sink: runtime.span_sink,
            drain: runtime.drain,
        };
        Self {
            config,
            runtime,
            stack: svc::stack(()),
        }
    }

    #[cfg(any(test, feature = "test-util"))]
    pub fn for_test() -> (Self, drain::Signal) {
        let (rt, drain) = test_util::runtime();
        let this = Self::new(test_util::default_config(), rt);
        (this, drain)
    }
}

impl<S> Outbound<S> {
    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn config_mut(&mut self) -> &mut Config {
        &mut self.config
    }

    pub fn metrics(&self) -> Metrics {
        self.runtime.metrics.clone()
    }

    pub fn stack_metrics(&self) -> metrics::Stack {
        self.runtime.metrics.proxy.stack.clone()
    }

    pub fn with_stack<Svc>(self, stack: Svc) -> Outbound<Svc> {
        self.map_stack(move |_, _, _| svc::stack(stack))
    }

    pub fn into_stack(self) -> svc::Stack<S> {
        self.stack
    }

    pub fn into_inner(self) -> S {
        self.stack.into_inner()
    }

    /// Wraps the inner `S`-typed stack in the given `L`-typed [`svc::Layer`].
    pub fn push<L: svc::Layer<S>>(self, layer: L) -> Outbound<L::Service> {
        self.map_stack(move |_, _, stack| stack.push(layer))
    }

    /// Creates a new `Outbound` by replacing the inner stack, as modified by `f`.
    fn map_stack<T>(
        self,
        f: impl FnOnce(&Config, &Runtime, svc::Stack<S>) -> svc::Stack<T>,
    ) -> Outbound<T> {
        let stack = f(&self.config, &self.runtime, self.stack);
        Outbound {
            config: self.config,
            runtime: self.runtime,
            stack,
        }
    }

    pub fn check_new_service<T, Req>(self) -> Self
    where
        S: svc::NewService<T> + Clone + Send + Sync + 'static,
        S::Service: svc::Service<Req>,
    {
        self
    }
}

impl Outbound<()> {
    pub async fn serve<T, I, P, R>(
        self,
        listen: impl Stream<Item = Result<(T, I)>> + Send + Sync + 'static,
        profiles: P,
        resolve: R,
    ) where
        // Target describing a server-side connection.
        T: Param<Remote<ClientAddr>>,
        T: Param<OrigDstAddr>,
        T: Clone + Send + Sync + 'static,
        // Server-side socket.
        I: io::AsyncRead + io::AsyncWrite + io::Peek + io::PeerAddr,
        I: Debug + Unpin + Send + Sync + 'static,
        // Configuration discovery.
        P: profiles::GetProfile<Error = Error>,
        // Endpoint resolution.
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>,
    {
        let profiles = discover::WithAllowlist::new(
            profiles.into_service(),
            self.config.allow_discovery.clone(),
        );
        if self.config.ingress_mode {
            tracing::info!("Outbound routing in ingress-mode");
            let server = self.mk_ingress(profiles, resolve);
            let shutdown = self.runtime.drain.signaled();
            serve::serve(listen, server, shutdown).await;
        } else {
            let proxy = self.mk_sidecar(profiles, todo!(), resolve);
            let shutdown = self.runtime.drain.signaled();
            serve::serve(listen, proxy, shutdown).await;
        }
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
