//! Configures and runs the outbound proxy.
//!
//! The outbound proxy is responsible for routing traffic from the local application to other hosts.

#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![allow(opaque_hidden_inferred_bound)]
#![forbid(unsafe_code)]

use futures::Stream;
use linkerd_app_core::{
    config::{ProxyConfig, QueueConfig},
    drain,
    exp_backoff::ExponentialBackoff,
    http_tracing::OpenCensusSink,
    identity, io, profiles,
    proxy::{
        self,
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
        tap,
    },
    serve,
    svc::{self, ServiceExt},
    tls,
    transport::addrs::*,
    AddrMatch, Error, ProxyRuntime, Result,
};
use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    net::IpAddr,
    num::NonZeroU16,
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

pub use self::{
    discover::{spawn_synthesized_profile_policy, synthesize_forward_policy, Discovery},
    metrics::OutboundMetrics,
};

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
    metrics: OutboundMetrics,
    identity: identity::NewClient,
    tap: tap::Registry,
    span_sink: OpenCensusSink,
    drain: drain::Watch,
}

pub type ConnectMeta = tls::ConnectMeta<Local<ClientAddr>>;

/// A reference to a frontend/apex resource, usually a service.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ParentRef(Arc<policy::Meta>);

/// A reference to a route resource, usually a service.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RouteRef(Arc<policy::Meta>);

/// A reference to a backend resource, usually a service.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BackendRef(Arc<policy::Meta>);

/// A reference to a backend resource, usually a service.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EndpointRef(Arc<policy::Meta>);

// === impl Outbound ===

impl Outbound<()> {
    pub fn new(config: Config, runtime: ProxyRuntime) -> Self {
        let runtime = Runtime {
            metrics: OutboundMetrics::new(runtime.metrics),
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

    pub fn build_policies<C>(
        &self,
        workload: Arc<str>,
        client: C,
        backoff: ExponentialBackoff,
    ) -> impl policy::GetPolicy
    where
        C: tonic::client::GrpcService<tonic::body::BoxBody, Error = Error>,
        C: Clone + Unpin + Send + Sync + 'static,
        C::ResponseBody: proxy::http::HttpBody<Data = tonic::codegen::Bytes, Error = Error>,
        C::ResponseBody: Default + Send + 'static,
        C::Future: Send,
    {
        policy::Api::new(workload, Duration::from_secs(10), client)
            .into_watch(backoff)
            .map_result(|response| match response {
                Err(e) => Err(e.into()),
                Ok(rsp) => Ok(rsp.into_inner()),
            })
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

    pub fn metrics(&self) -> OutboundMetrics {
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
    pub async fn serve<T, I, R>(
        self,
        listen: impl Stream<Item = Result<(T, I)>> + Send + Sync + 'static,
        profiles: impl profiles::GetProfile<Error = Error>,
        policies: impl policy::GetPolicy,
        resolve: R,
    ) where
        // Target describing a server-side connection.
        T: svc::Param<AddrPair>,
        T: svc::Param<OrigDstAddr>,
        T: Clone + Send + Sync + 'static,
        // Server-side socket.
        I: io::AsyncRead + io::AsyncWrite + io::Peek + io::PeerAddr,
        I: Debug + Unpin + Send + Sync + 'static,
        // Endpoint resolution.
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>,
    {
        let profiles = profiles::WithAllowlist::new(profiles, self.config.allow_discovery.clone());
        if self.config.ingress_mode {
            tracing::info!("Outbound routing in ingress-mode");
            let server = self.mk_ingress(profiles, policies, resolve);
            let shutdown = self.runtime.drain.signaled();
            serve::serve(listen, server, shutdown).await;
        } else {
            let proxy = self.mk_sidecar(profiles, policies, resolve);
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

// === impl ParentRef ===

impl std::ops::Deref for ParentRef {
    type Target = policy::Meta;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<EndpointRef> for ParentRef {
    fn from(EndpointRef(meta): EndpointRef) -> Self {
        Self(meta)
    }
}

// === impl RouteRef ===

impl std::ops::Deref for RouteRef {
    type Target = policy::Meta;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

// === impl BackendRef ===

impl std::ops::Deref for BackendRef {
    type Target = policy::Meta;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<ParentRef> for BackendRef {
    fn from(ParentRef(meta): ParentRef) -> Self {
        Self(meta)
    }
}

impl From<EndpointRef> for BackendRef {
    fn from(EndpointRef(meta): EndpointRef) -> Self {
        Self(meta)
    }
}

// === impl EndpointRef ===

impl std::ops::Deref for EndpointRef {
    type Target = policy::Meta;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl EndpointRef {
    fn new(md: &Metadata, port: NonZeroU16) -> Self {
        let namespace = match md.labels().get("dst_namespace") {
            Some(ns) => ns.clone(),
            None => return Self(UNKNOWN_META.clone()),
        };
        let name = match md.labels().get("dst_pod") {
            Some(pod) => pod.clone(),
            None => return Self(UNKNOWN_META.clone()),
        };
        Self(Arc::new(policy::Meta::Resource {
            group: "core".to_string(),
            kind: "Pod".to_string(),
            namespace,
            name,
            section: None,
            port: Some(port),
        }))
    }
}

static UNKNOWN_META: once_cell::sync::Lazy<Arc<policy::Meta>> =
    once_cell::sync::Lazy::new(|| policy::Meta::new_default("unknown"));
