//! Configures and runs the inbound proxy.
//!
//! The inbound proxy is responsible for terminating traffic from other network
//! endpoints inbound to the local application.

#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]

mod accept;
mod detect;
pub mod direct;
mod http;
mod metrics;
pub mod policy;
mod server;
#[cfg(any(test, fuzzing))]
pub(crate) mod test_util;

pub use self::{metrics::Metrics, policy::DefaultPolicy};
use linkerd_app_core::{
    config::{ConnectConfig, ProxyConfig},
    drain,
    http_tracing::OpenCensusSink,
    io,
    proxy::tcp,
    proxy::{identity::LocalCrtKey, tap},
    svc,
    transport::{self, Remote, ServerAddr},
    Error, NameMatch, Runtime as AppRuntime,
};
use std::{fmt::Debug, time::Duration};
use tracing::debug_span;

#[cfg(fuzzing)]
pub use self::http::fuzz as http_fuzz;

#[derive(Clone, Debug)]
pub struct Config {
    pub allow_discovery: NameMatch,
    pub proxy: ProxyConfig,
    pub policy: policy::Config,
    pub profile_idle_timeout: Duration,
}

#[derive(Clone)]
pub struct Inbound<S> {
    config: Config,
    runtime: Runtime,
    stack: svc::Stack<S>,
}

#[derive(Clone)]
struct Runtime {
    metrics: Metrics,
    identity: Option<LocalCrtKey>,
    tap: tap::Registry,
    span_sink: OpenCensusSink,
    drain: drain::Watch,
}

// === impl Inbound ===

impl<S> Inbound<S> {
    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn identity(&self) -> Option<&LocalCrtKey> {
        self.runtime.identity.as_ref()
    }

    pub fn proxy_metrics(&self) -> &metrics::Proxy {
        &self.runtime.metrics.proxy
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
    pub fn new(config: Config, metrics: Metrics, runtime: AppRuntime) -> Self {
        let runtime = Runtime {
            metrics,
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

    pub fn with_stack<S>(self, stack: S) -> Inbound<S> {
        self.map_stack(move |_, _, _| svc::stack(stack))
    }

    /// Readies the inbound stack to make TCP connections (for both TCP
    // forwarding and HTTP proxying).
    pub fn into_tcp_connect<T>(
        self,
        proxy_port: u16,
    ) -> Inbound<
        impl svc::Service<
                T,
                Response = impl io::AsyncRead + io::AsyncWrite + Send,
                Error = Error,
                Future = impl Send,
            > + Clone,
    >
    where
        T: svc::Param<u16> + 'static,
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
                .push_request_filter(move |t: T| {
                    let port = t.param();
                    if port == proxy_port {
                        return Err(Loop(port));
                    }
                    Ok(Remote(ServerAddr(([127, 0, 0, 1], port).into())))
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
        svc::BoxNewService<
            T,
            impl svc::Service<I, Response = (), Error = Error, Future = impl Send> + Clone,
        >,
    >
    where
        T: svc::Param<transport::labels::Key> + Clone + Send + 'static,
        I: io::AsyncRead + io::AsyncWrite,
        I: Debug + Send + Unpin + 'static,
        S: svc::Service<T> + Clone + Send + Sync + Unpin + 'static,
        S::Response: io::AsyncRead + io::AsyncWrite + Send + Unpin + 'static,
        S::Error: Into<Error>,
        S::Future: Send,
    {
        self.map_stack(|_, rt, connect| {
            connect
                .push(transport::metrics::Client::layer(
                    rt.metrics.proxy.transport.clone(),
                ))
                .push_make_thunk()
                .push_on_response(
                    svc::layers()
                        .push(tcp::Forward::layer())
                        .push(drain::Retain::layer(rt.drain.clone())),
                )
                .instrument(|_: &_| debug_span!("tcp"))
                .push(svc::BoxNewService::layer())
                .check_new::<T>()
        })
    }
}

fn stack_labels(proto: &'static str, name: &'static str) -> metrics::StackLabels {
    metrics::StackLabels::inbound(proto, name)
}
