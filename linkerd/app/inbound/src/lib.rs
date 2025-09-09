//! Configures and runs the inbound proxy.
//!
//! The inbound proxy is responsible for terminating traffic from other network
//! endpoints inbound to the local application.

#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![allow(opaque_hidden_inferred_bound)]
#![forbid(unsafe_code)]

mod accept;
mod detect;
pub mod direct;
mod http;
mod metrics;
pub mod policy;
mod server;

#[cfg(any(test, feature = "test-util", fuzzing))]
pub mod test_util;

#[cfg(fuzzing)]
pub use self::http::fuzz as http_fuzz;
pub use self::{
    detect::MetricsFamilies as DetectMetrics, metrics::InboundMetrics, policy::DefaultPolicy,
};
use linkerd_app_core::{
    config::{ProxyConfig, QueueConfig},
    drain,
    http_tracing::SpanSink,
    identity,
    metrics::prom,
    proxy::tap,
    svc,
    transport::{self, Remote, ServerAddr},
    Error, NameAddr, NameMatch, ProxyRuntime,
};
use std::{fmt::Debug, time::Duration};
use thiserror::Error;

#[derive(Clone, Debug)]
pub struct Config {
    pub allow_discovery: NameMatch,
    pub proxy: ProxyConfig,
    pub policy: policy::Config,
    pub allowed_ips: transport::AllowIps,

    /// Configures the timeout after which the proxy will revert to skipping
    /// service profile routing instrumentation.
    pub profile_skip_timeout: Duration,

    /// Configures how long the proxy will retain policy & profile resolutions
    /// for idle/unused ports and services.
    pub discovery_idle_timeout: Duration,

    /// Configures how HTTP requests are buffered *for each inbound port*.
    pub http_request_queue: QueueConfig,

    /// Enables unsafe authority labels.
    pub unsafe_authority_labels: bool,
}

#[derive(Clone)]
pub struct Inbound<S> {
    config: Config,
    runtime: Runtime,
    stack: svc::Stack<S>,
}

#[derive(Clone)]
struct Runtime {
    metrics: InboundMetrics,
    identity: identity::creds::Receiver,
    tap: tap::Registry,
    span_sink: Option<SpanSink>,
    drain: drain::Watch,
}

/// Indicates the name to be used to route gateway connections.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct GatewayAddr(pub NameAddr);

// The inbound HTTP server handles gateway traffic; so gateway error types are defined here (so that
// error metrics can be recorded properly).

#[derive(Debug, Error)]
#[error("no identity provided")]
pub struct GatewayIdentityRequired;

#[derive(Debug, Error)]
#[error("bad gateway domain")]
pub struct GatewayDomainInvalid;

#[derive(Debug, Error)]
#[error("gateway loop detected")]
pub struct GatewayLoop;

#[derive(Debug, thiserror::Error)]
#[error("server {addr}: {source}")]
pub struct ForwardError {
    addr: Remote<ServerAddr>,
    source: Error,
}

// === impl Inbound ===

impl<S> Inbound<S> {
    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn identity(&self) -> &identity::creds::Receiver {
        &self.runtime.identity
    }

    /// A helper for gateways to instrument policy checks.
    pub fn authorize_http<N>(
        &self,
    ) -> impl svc::layer::Layer<N, Service = policy::NewHttpPolicy<N>> + Clone {
        policy::NewHttpPolicy::layer(self.runtime.metrics.http_authz.clone())
    }

    /// A helper for gateways to instrument policy checks.
    pub fn authorize_tcp<N>(
        &self,
    ) -> impl svc::layer::Layer<N, Service = policy::NewTcpPolicy<N>> + Clone {
        policy::NewTcpPolicy::layer(self.runtime.metrics.tcp_authz.clone())
    }

    pub fn into_stack(self) -> svc::Stack<S> {
        self.stack
    }

    pub fn into_inner(self) -> S {
        self.stack.into_inner()
    }

    /// Creates a new `Inbound` by replacing the inner stack, as modified by `f`.
    fn map_stack<T>(
        self,
        f: impl FnOnce(&Config, &Runtime, svc::Stack<S>) -> svc::Stack<T>,
    ) -> Inbound<T> {
        let stack = f(&self.config, &self.runtime, self.stack);
        Inbound {
            config: self.config,
            runtime: self.runtime,
            stack,
        }
    }
}

impl Inbound<()> {
    pub fn new(config: Config, runtime: ProxyRuntime, prom: &mut prom::Registry) -> Self {
        let runtime = Runtime {
            metrics: InboundMetrics::new(runtime.metrics, prom),
            identity: runtime.identity,
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
        let this = Self::new(
            test_util::default_config(),
            rt,
            &mut prom::Registry::default(),
        );
        (this, drain)
    }

    pub fn metrics(&self) -> InboundMetrics {
        self.runtime.metrics.clone()
    }

    pub fn with_stack<S>(self, stack: S) -> Inbound<S> {
        self.map_stack(move |_, _, _| svc::stack(stack))
    }
}

impl<S> Inbound<S> {
    pub fn push<L: svc::layer::Layer<S>>(self, layer: L) -> Inbound<L::Service> {
        self.map_stack(|_, _, stack| stack.push(layer))
    }
}

fn stack_labels(proto: &'static str, name: &'static str) -> metrics::StackLabels {
    metrics::StackLabels::inbound(proto, name)
}

impl<T> From<(&T, Error)> for ForwardError
where
    T: svc::Param<Remote<ServerAddr>>,
{
    fn from((target, source): (&T, Error)) -> Self {
        Self {
            addr: target.param(),
            source,
        }
    }
}
