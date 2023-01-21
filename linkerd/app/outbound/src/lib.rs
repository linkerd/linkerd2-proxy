//! Configures and runs the outbound proxy.
//!
//! The outbound proxy is responsible for routing traffic from the local application to other hosts.

#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

mod discover;
pub mod endpoint;
pub mod http;
mod ingress;
pub mod logical;
mod metrics;
mod switch_logical;
pub mod tcp;
#[cfg(test)]
pub(crate) mod test_util;

pub use self::metrics::Metrics;
use futures::Stream;
use linkerd_app_core::{
    config::{ProxyConfig, QueueConfig},
    drain,
    http_tracing::OpenCensusSink,
    identity, io, profiles,
    proxy::{api_resolve::Metadata, core::Resolve, tap},
    serve,
    svc::{self, stack::Param},
    tls,
    transport::{self, addrs::*},
    AddrMatch, Error, ProxyRuntime, Result,
};
use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    net::IpAddr,
    sync::Arc,
    time::Duration,
};
use tracing::{info, info_span};

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
    pub tcp_connection_buffer: QueueConfig,

    /// Configures how HTTP requests are buffered *for each outbound address*.
    ///
    /// A buffer capacity of 100 means that 100 requests may be buffered for
    /// each IP:port to which an application has opened an outbound TCP connection.
    pub http_request_buffer: QueueConfig,

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

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct Accept<P> {
    pub orig_dst: OrigDstAddr,
    pub protocol: P,
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

    pub fn metrics(&self) -> Metrics {
        self.runtime.metrics.clone()
    }

    pub fn with_stack<S>(self, stack: S) -> Outbound<S> {
        self.map_stack(move |_, _, _| svc::stack(stack))
    }
}

impl<S> Outbound<S> {
    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn into_stack(self) -> svc::Stack<S> {
        self.stack
    }

    pub fn into_inner(self) -> S {
        self.stack.into_inner()
    }

    fn no_tls_reason(&self) -> tls::NoClientTls {
        tls::NoClientTls::NotProvidedByServiceDiscovery
    }

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
}

impl Outbound<()> {
    pub async fn serve<A, I, P, R>(
        self,
        listen: impl Stream<Item = Result<(A, I)>> + Send + Sync + 'static,
        profiles: P,
        resolve: R,
    ) where
        A: Param<Remote<ClientAddr>> + Param<OrigDstAddr> + Clone + Send + Sync + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::Peek + io::PeerAddr,
        I: Debug + Unpin + Send + Sync + 'static,
        R: Clone + Send + Sync + Unpin + 'static,
        R: Resolve<tcp::Concrete, Endpoint = Metadata, Error = Error>,
        <R as Resolve<tcp::Concrete>>::Resolution: Send,
        <R as Resolve<tcp::Concrete>>::Future: Send + Unpin,
        R: Resolve<http::Concrete, Endpoint = Metadata, Error = Error>,
        <R as Resolve<http::Concrete>>::Resolution: Send,
        <R as Resolve<http::Concrete>>::Future: Send + Unpin,
        P: profiles::GetProfile<profiles::LookupAddr> + Clone + Send + Sync + Unpin + 'static,
        P::Future: Send,
        P::Error: Send,
    {
        if self.config.ingress_mode {
            info!("Outbound routing in ingress-mode");
            let server = self.mk_ingress(profiles, resolve);
            let shutdown = self.runtime.drain.signaled();
            serve::serve(listen, server, shutdown).await;
        } else {
            let proxy = self.mk_proxy(profiles, resolve);
            let shutdown = self.runtime.drain.signaled();
            serve::serve(listen, proxy, shutdown).await;
        }
    }

    fn mk_proxy<T, I, P, R>(&self, profiles: P, resolve: R) -> svc::ArcNewTcp<T, I>
    where
        T: Param<OrigDstAddr> + Clone + Send + Sync + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::Peek + io::PeerAddr,
        I: Debug + Unpin + Send + Sync + 'static,
        R: Clone + Send + Sync + Unpin + 'static,
        R: Resolve<tcp::Concrete, Endpoint = Metadata, Error = Error>,
        <R as Resolve<tcp::Concrete>>::Resolution: Send,
        <R as Resolve<tcp::Concrete>>::Future: Send + Unpin,
        R: Resolve<http::Concrete, Endpoint = Metadata, Error = Error>,
        <R as Resolve<http::Concrete>>::Resolution: Send,
        <R as Resolve<http::Concrete>>::Future: Send + Unpin,
        P: profiles::GetProfile<profiles::LookupAddr> + Clone + Send + Sync + Unpin + 'static,
        P::Future: Send,
        P::Error: Send,
    {
        let logical = self.to_tcp_connect().push_logical(resolve);
        let forward = self.to_tcp_connect().push_forward();
        forward
            .push_switch_logical(logical.into_inner())
            .push_discover(profiles)
            .push_tcp_instrument(|t: &T| info_span!("proxy", addr = %t.param()))
            .into_inner()
    }

    /// Builds a an "ingress mode" proxy.
    ///
    /// Ingress-mode proxies route based on request headers instead of using the
    /// original destination. Protocol detection is **always** performed. If it
    /// fails, we revert to using the normal IP-based discovery
    fn mk_ingress<T, I, P, R>(&self, profiles: P, resolve: R) -> svc::ArcNewTcp<T, I>
    where
        T: Param<OrigDstAddr> + Clone + Send + Sync + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::Peek + io::PeerAddr,
        I: Debug + Unpin + Send + Sync + 'static,
        R: Clone + Send + Sync + Unpin + 'static,
        R: Resolve<tcp::Concrete, Endpoint = Metadata, Error = Error>,
        <R as Resolve<tcp::Concrete>>::Resolution: Send,
        <R as Resolve<tcp::Concrete>>::Future: Send + Unpin,
        R: Resolve<http::Concrete, Endpoint = Metadata, Error = Error>,
        <R as Resolve<http::Concrete>>::Resolution: Send,
        <R as Resolve<http::Concrete>>::Future: Send + Unpin,
        P: profiles::GetProfile<profiles::LookupAddr> + Clone + Send + Sync + Unpin + 'static,
        P::Future: Send,
        P::Error: Send,
    {
        // The fallback stack is the same thing as the normal proxy stack, but
        // it doesn't include TCP metrics, since they are already instrumented
        // on this ingress stack.
        let fallback = {
            let logical = self.to_tcp_connect().push_logical(resolve.clone());
            let forward = self.to_tcp_connect().push_forward();
            forward
                .push_switch_logical(logical.into_inner())
                .push_discover(profiles.clone())
                .into_inner()
        };

        self.to_tcp_connect()
            .push_tcp_endpoint()
            .push_http_endpoint()
            .push_ingress(profiles, resolve, fallback)
            .push_tcp_instrument(|t: &T| info_span!("ingress", addr = %t.param()))
            .into_inner()
    }
}

// === impl Accept ===

impl<P> Param<transport::labels::Key> for Accept<P> {
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::outbound_server(self.orig_dst.into())
    }
}

impl<P> Param<OrigDstAddr> for Accept<P> {
    fn param(&self) -> OrigDstAddr {
        self.orig_dst
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
