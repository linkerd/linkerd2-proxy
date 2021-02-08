//! Configures and runs the inbound proxy.
//!
//! The inbound proxy is responsible for terminating traffic from other network
//! endpoints inbound to the local application.

#![deny(warnings, rust_2018_idioms)]

mod allow_discovery;
pub mod direct;
pub mod http;
mod prevent_loop;
mod require_identity;
pub mod target;
#[cfg(test)]
pub(crate) mod test_util;

pub use self::target::{HttpEndpoint, Logical, RequestTarget, Target, TcpEndpoint};
use self::{
    prevent_loop::PreventLoop,
    require_identity::RequireIdentityForPorts,
    target::{HttpAccept, TcpAccept},
};
use linkerd_app_core::{
    config::{ConnectConfig, ProxyConfig},
    detect, drain, io, metrics, profiles,
    proxy::tcp,
    svc::{self, stack::Param},
    tls,
    transport::{self, listen},
    Error, NameMatch, ProxyRuntime,
};
use std::{fmt::Debug, time::Duration};
use tracing::debug_span;

#[derive(Clone, Debug)]
pub struct Config {
    pub allow_discovery: NameMatch,
    pub proxy: ProxyConfig,
    pub require_identity_for_inbound_ports: RequireIdentityForPorts,
    pub disable_protocol_detection_for_ports: SkipByPort,
    pub profile_idle_timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct SkipByPort(std::sync::Arc<indexmap::IndexSet<u16>>);

#[derive(Clone, Debug)]
pub struct Inbound<S> {
    config: Config,
    runtime: ProxyRuntime,
    stack: svc::Stack<S>,
}

// === impl Config ===

impl<S> Inbound<S> {
    fn new(config: Config, runtime: ProxyRuntime, stack: S) -> Self {
        Self {
            config,
            runtime,
            stack: svc::stack(stack),
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn runtime(&self) -> &ProxyRuntime {
        &self.runtime
    }

    pub fn into_inner(self) -> S {
        self.stack.into_inner()
    }
}

impl Inbound<()> {
    pub fn new_without_stack(config: Config, runtime: ProxyRuntime) -> Self {
        Self::new(config, runtime, ())
    }

    pub fn with_stack<S>(self, stack: svc::Stack<S>) -> Inbound<S> {
        Inbound {
            config: self.config,
            runtime: self.runtime,
            stack,
        }
    }

    pub fn tcp_connect<T: Param<u16>>(
        self,
    ) -> Inbound<
        impl svc::Service<
                T,
                Response = impl io::AsyncRead + io::AsyncWrite + Send,
                Error = Error,
                Future = impl Send,
            > + Clone,
    > {
        let Self {
            config, runtime, ..
        } = self;

        // Establishes connections to remote peers (for both TCP
        // forwarding and HTTP proxying).
        let ConnectConfig {
            keepalive, timeout, ..
        } = config.proxy.connect;

        let stack = svc::stack(transport::ConnectTcp::new(keepalive))
            .push_map_target(|t: T| transport::ConnectAddr(([127, 0, 0, 1], t.param()).into()))
            // Limits the time we wait for a connection to be established.
            .push_timeout(timeout)
            .push(svc::stack::BoxFuture::layer());

        Inbound {
            config,
            runtime,
            stack,
        }
    }
}

impl<C> Inbound<C>
where
    C: svc::Service<TcpEndpoint> + Clone + Send + Sync + Unpin + 'static,
    C::Response: io::AsyncRead + io::AsyncWrite + Send + Unpin + 'static,
    C::Error: Into<Error>,
    C::Future: Send + Unpin,
{
    pub fn push_tcp_forward<I>(
        self,
        server_port: u16,
    ) -> Inbound<
        impl svc::NewService<
                TcpEndpoint,
                Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send> + Clone,
            > + Clone,
    >
    where
        I: io::AsyncRead + io::AsyncWrite,
        I: Debug + Send + Sync + Unpin + 'static,
    {
        let Self {
            config,
            runtime: rt,
            stack: connect,
        } = self;
        let prevent_loop = PreventLoop::from(server_port);

        // Forwards TCP streams that cannot be decoded as HTTP.
        //
        // Looping is always prevented.
        let stack = connect
            .push_request_filter(prevent_loop)
            .push(rt.metrics.transport.layer_connect())
            .push_make_thunk()
            .push_on_response(
                svc::layers()
                    .push(tcp::Forward::layer())
                    .push(drain::Retain::layer(rt.drain.clone())),
            )
            .instrument(|_: &_| debug_span!("tcp"))
            .check_new::<TcpEndpoint>();

        Inbound {
            config,
            runtime: rt,
            stack,
        }
    }

    pub fn into_server<I, G, GSvc, P>(
        self,
        server_port: u16,
        profiles: P,
        gateway: G,
    ) -> impl svc::NewService<
        listen::Addrs,
        Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send>,
    > + Clone
    where
        I: io::AsyncRead + io::AsyncWrite + io::Peek + io::PeerAddr,
        I: Debug + Send + Sync + Unpin + 'static,
        G: svc::NewService<direct::GatewayConnection, Service = GSvc>,
        G: Clone + Send + Sync + Unpin + 'static,
        GSvc: svc::Service<direct::GatewayIo<I>, Response = ()> + Send + 'static,
        GSvc::Error: Into<Error>,
        GSvc::Future: Send,
        P: profiles::GetProfile<profiles::LogicalAddr> + Clone + Send + Sync + 'static,
        P::Error: Send,
        P::Future: Send,
    {
        let disable_detect = self.config.disable_protocol_detection_for_ports.clone();
        let require_id = self.config.require_identity_for_inbound_ports.clone();
        let config = self.config.proxy.clone();
        self.clone()
            .push_http_router(profiles)
            .push_http_server()
            .stack
            .push_map_target(HttpAccept::from)
            .push(svc::UnwrapOr::layer(
                // When HTTP detection fails, forward the connection to the
                // application as an opaque TCP stream.
                self.clone()
                    .push_tcp_forward(server_port)
                    .stack
                    .push_map_target(TcpEndpoint::from)
                    .into_inner(),
            ))
            .push_cache(config.cache_max_idle_age)
            .push(detect::NewDetectService::layer(
                config.detect_protocol_timeout,
                http::DetectHttp::default(),
            ))
            .push_request_filter(require_id)
            .push(self.runtime.metrics.transport.layer_accept())
            .push_map_target(TcpAccept::from)
            .push(tls::NewDetectTls::layer(
                self.runtime.identity.clone(),
                config.detect_protocol_timeout,
            ))
            .check_new_service::<listen::Addrs, I>()
            .push_switch(
                disable_detect,
                self.clone()
                    .push_tcp_forward(server_port)
                    .stack
                    .push_map_target(TcpEndpoint::from)
                    .push(self.runtime.metrics.transport.layer_accept())
                    .push_map_target(TcpAccept::port_skipped)
                    .check_new_service::<listen::Addrs, I>()
                    .into_inner(),
            )
            .check_new_service::<listen::Addrs, I>()
            .push_switch(
                PreventLoop::from(server_port),
                self.push_tcp_forward(server_port)
                    .push_direct(gateway)
                    .stack
                    .check_new_service::<listen::Addrs, I>()
                    .into_inner(),
            )
            .check_new_service::<listen::Addrs, I>()
            .into_inner()
    }
}

// === impl SkipByPort ===

impl From<indexmap::IndexSet<u16>> for SkipByPort {
    fn from(ports: indexmap::IndexSet<u16>) -> Self {
        SkipByPort(ports.into())
    }
}

impl svc::Predicate<listen::Addrs> for SkipByPort {
    type Request = svc::Either<listen::Addrs, listen::Addrs>;

    fn check(&mut self, t: listen::Addrs) -> Result<Self::Request, Error> {
        if !self.0.contains(&t.target_addr().port()) {
            Ok(svc::Either::A(t))
        } else {
            Ok(svc::Either::B(t))
        }
    }
}

fn stack_labels(proto: &'static str, name: &'static str) -> metrics::StackLabels {
    metrics::StackLabels::inbound(proto, name)
}
