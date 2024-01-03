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

pub use self::{metrics::InboundMetrics, policy::DefaultPolicy};
use linkerd_app_core::{
    config::{ConnectConfig, ProxyConfig, QueueConfig},
    drain,
    http_tracing::OpenCensusSink,
    identity, io,
    proxy::{tap, tcp},
    svc,
    transport::{self, Remote, ServerAddr},
    Error, NameAddr, NameMatch, ProxyRuntime,
};
use std::{fmt::Debug, time::Duration};
use thiserror::Error;
use tracing::debug_span;

#[cfg(fuzzing)]
pub use self::http::fuzz as http_fuzz;

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
    span_sink: OpenCensusSink,
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

    pub fn proxy_metrics(&self) -> &metrics::Proxy {
        &self.runtime.metrics.proxy
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
    pub fn new(config: Config, runtime: ProxyRuntime) -> Self {
        let runtime = Runtime {
            metrics: InboundMetrics::new(runtime.metrics),
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
        let this = Self::new(test_util::default_config(), rt);
        (this, drain)
    }

    pub fn metrics(&self) -> InboundMetrics {
        self.runtime.metrics.clone()
    }

    pub fn with_stack<S>(self, stack: S) -> Inbound<S> {
        self.map_stack(move |_, _, _| svc::stack(stack))
    }

    /// Readies the inbound stack to make TCP connections (for both TCP
    // forwarding and HTTP proxying).
    pub fn into_tcp_connect<T>(
        self,
        proxy_port: u16,
    ) -> Inbound<
        impl svc::MakeConnection<
                T,
                Connection = impl Send + Unpin,
                Metadata = impl Send + Unpin,
                Error = Error,
                Future = impl Send,
            > + Clone,
    >
    where
        T: svc::Param<Remote<ServerAddr>> + 'static,
    {
        self.map_stack(|config, _, _| {
            // Establishes connections to remote peers (for both TCP
            // forwarding and HTTP proxying).
            let ConnectConfig {
                ref keepalive,
                ref timeout,
                ..
            } = config.proxy.connect;

            #[derive(Debug, thiserror::Error)]
            #[error("inbound connection must not target port {0}")]
            struct Loop(u16);

            svc::stack(transport::ConnectTcp::new(*keepalive))
                // Limits the time we wait for a connection to be established.
                .push_connect_timeout(*timeout)
                // Prevent connections that would target the inbound proxy port from looping.
                .push_filter(move |t: T| {
                    let addr = t.param();
                    let port = addr.port();
                    if port == proxy_port {
                        return Err(Loop(port));
                    }
                    Ok(addr)
                })
        })
    }
}

impl<S> Inbound<S> {
    pub fn push<L: svc::layer::Layer<S>>(self, layer: L) -> Inbound<L::Service> {
        self.map_stack(|_, _, stack| stack.push(layer))
    }

    // Forwards TCP streams that cannot be decoded as HTTP.
    //
    // Looping is always prevented.
    pub fn push_tcp_forward<T, I>(
        self,
    ) -> Inbound<
        svc::ArcNewService<
            T,
            impl svc::Service<I, Response = (), Error = ForwardError, Future = impl Send> + Clone,
        >,
    >
    where
        T: svc::Param<transport::labels::Key>
            + svc::Param<Remote<ServerAddr>>
            + Clone
            + Send
            + Sync
            + 'static,
        I: io::AsyncRead + io::AsyncWrite,
        I: Debug + Send + Unpin + 'static,
        S: svc::MakeConnection<T> + Clone + Send + Sync + Unpin + 'static,
        S::Connection: Send + Unpin,
        S::Metadata: Send + Unpin,
        S::Future: Send,
    {
        self.map_stack(|_, rt, connect| {
            connect
                .push(transport::metrics::Client::layer(
                    rt.metrics.proxy.transport.clone(),
                ))
                .push(svc::stack::WithoutConnectionMetadata::layer())
                .push_new_thunk()
                .push_on_service(tcp::Forward::layer())
                .push_on_service(drain::Retain::layer(rt.drain.clone()))
                .instrument(|_: &_| debug_span!("tcp"))
                .push(svc::NewMapErr::layer_from_target::<ForwardError, _>())
                .push(svc::ArcNewService::layer())
                .check_new::<T>()
        })
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
