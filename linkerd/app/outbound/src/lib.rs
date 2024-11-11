//! Configures and runs the outbound proxy.
//!
//! The outbound proxy is responsible for routing traffic from the local application to other hosts.

#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![allow(opaque_hidden_inferred_bound)]
#![forbid(unsafe_code)]

use linkerd_app_core::http_tracing::SpanSink;
use linkerd_app_core::{
    config::{ProxyConfig, QueueConfig},
    drain,
    exp_backoff::ExponentialBackoff,
    identity, io,
    metrics::prom,
    profiles,
    proxy::{
        self,
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
        tap,
    },
    svc::{self, ServiceExt},
    tls::ConnectMeta as TlsConnectMeta,
    transport::addrs::*,
    AddrMatch, Error, NameAddr, ProxyRuntime,
};
use linkerd_tonic_stream::ReceiveLimits;
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
pub mod tls;
mod zone;

use self::metrics::OutboundMetrics;
pub use self::{
    discover::{spawn_synthesized_profile_policy, synthesize_forward_policy, Discovery},
    policy::{BackendRef, EndpointRef, ParentRef, RouteRef},
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
    span_sink: Option<SpanSink>,
    drain: drain::Watch,
}

pub type ConnectMeta = TlsConnectMeta<Local<ClientAddr>>;

// === impl Outbound ===

impl Outbound<()> {
    pub fn new(config: Config, runtime: ProxyRuntime, prom: &mut prom::Registry) -> Self {
        let runtime = Runtime {
            metrics: OutboundMetrics::new(runtime.metrics, prom),
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
        limits: ReceiveLimits,
    ) -> impl policy::GetPolicy
    where
        C: tonic::client::GrpcService<tonic::body::BoxBody, Error = Error>,
        C: Clone + Unpin + Send + Sync + 'static,
        C::ResponseBody: proxy::http::HttpBody<Data = tonic::codegen::Bytes, Error = Error>,
        C::ResponseBody: Default + Send + 'static,
        C::Future: Send,
    {
        policy::Api::new(workload, limits, Duration::from_secs(10), client)
            .into_watch(backoff)
            .map_result(|response| match response {
                Err(e) => Err(e.into()),
                Ok(rsp) => Ok(rsp.into_inner()),
            })
    }

    #[cfg(any(test, feature = "test-util"))]
    pub fn for_test() -> (Self, drain::Signal) {
        let (rt, drain) = test_util::runtime();
        let this = Self::new(test_util::default_config(), rt, &mut Default::default());
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
    pub fn mk<T, I, R>(
        self,
        profiles: impl profiles::GetProfile<Error = Error>,
        policies: impl policy::GetPolicy,
        resolve: R,
    ) -> svc::ArcNewTcp<T, I>
    where
        // Target describing a server-side connection.
        T: svc::Param<OrigDstAddr>,
        T: Clone + Send + Sync + 'static,
        // Server-side socket.
        I: io::AsyncRead + io::AsyncWrite + io::Peek + io::PeerAddr,
        I: Debug + Unpin + Send + Sync + 'static,
        // Endpoint resolution.
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>,
        R::Resolution: Unpin,
    {
        let profiles = profiles::WithAllowlist::new(profiles, self.config.allow_discovery.clone());
        if self.config.ingress_mode {
            tracing::info!("Outbound routing in ingress-mode");
            self.mk_ingress(profiles, policies, resolve)
        } else {
            self.mk_sidecar(profiles, policies, resolve)
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

pub(crate) fn service_meta(addr: &NameAddr) -> Option<Arc<policy::Meta>> {
    let mut parts = addr.name().split('.');

    let name = parts.next()?;
    let namespace = parts.next()?;

    if !parts.next()?.eq_ignore_ascii_case("svc") {
        return None;
    }

    Some(Arc::new(policy::Meta::Resource {
        group: "core".to_string(),
        kind: "Service".to_string(),
        namespace: namespace.to_string(),
        name: name.to_string(),
        section: None,
        port: Some(addr.port().try_into().ok()?),
    }))
}
