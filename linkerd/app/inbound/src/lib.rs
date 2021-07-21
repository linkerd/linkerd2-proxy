//! Configures and runs the inbound proxy.
//!
//! The inbound proxy is responsible for terminating traffic from other network
//! endpoints inbound to the local application.

#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]
#![allow(clippy::inconsistent_struct_constructor)]

mod allow_discovery;
pub mod direct;
pub mod http;
mod prevent_loop;
mod require_identity;
pub mod target;
#[cfg(any(test, fuzzing))]
pub(crate) mod test_util;

pub use self::target::{HttpEndpoint, Logical, RequestTarget, Target, TcpEndpoint};
use self::{
    prevent_loop::PreventLoop,
    require_identity::RequireIdentityForPorts,
    target::{HttpAccept, TcpAccept},
};
use linkerd_app_core::{
    config::{ConnectConfig, PortSet, ProxyConfig, ServerConfig},
    detect, drain, io, metrics, profiles,
    proxy::tcp,
    serve, svc, tls,
    transport::{self, listen::Bind, ClientAddr, Local, OrigDstAddr, Remote, ServerAddr},
    Error, NameMatch, Never, ProxyRuntime,
};
use std::{convert::TryFrom, fmt::Debug, future::Future, time::Duration};
use tracing::{debug_span, info_span};

#[derive(Clone, Debug)]
pub struct Config {
    pub allow_discovery: NameMatch,
    pub proxy: ProxyConfig,
    pub require_identity_for_inbound_ports: RequireIdentityForPorts,
    pub disable_protocol_detection_for_ports: PortSet,
    pub profile_idle_timeout: Duration,
}

#[derive(Clone)]
pub struct Inbound<S> {
    config: Config,
    runtime: ProxyRuntime,
    stack: svc::Stack<S>,
}

// === impl Inbound ===

impl<S> Inbound<S> {
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

    /// Creates a new `Inbound` by replacing the inner stack, as modified by `f`.
    fn map_stack<T>(
        self,
        f: impl FnOnce(&Config, &ProxyRuntime, svc::Stack<S>) -> svc::Stack<T>,
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
    pub fn to_tcp_connect<T: svc::Param<u16> + 'static>(
        &self,
    ) -> Inbound<
        impl svc::Service<
                T,
                Response = impl io::AsyncRead + io::AsyncWrite + Send,
                Error = Error,
                Future = impl Send,
            > + Clone,
    > {
        self.clone().map_stack(|config, _, _| {
            // Establishes connections to remote peers (for both TCP
            // forwarding and HTTP proxying).
            let ConnectConfig {
                keepalive, timeout, ..
            } = config.proxy.connect.clone();

            svc::stack(transport::ConnectTcp::new(keepalive))
                .push_map_target(|t: T| Remote(ServerAddr(([127, 0, 0, 1], t.param()).into())))
                // Limits the time we wait for a connection to be established.
                .push_connect_timeout(timeout)
                .push(svc::stack::BoxFuture::layer())
        })
    }

    pub fn serve<B, G, GSvc, P>(
        self,
        bind: B,
        profiles: P,
        gateway: G,
    ) -> (Local<ServerAddr>, impl Future<Output = ()> + Send)
    where
        B: Bind<ServerConfig>,
        B::Addrs: svc::Param<Remote<ClientAddr>>
            + svc::Param<Local<ServerAddr>>
            + svc::Param<OrigDstAddr>,
        G: svc::NewService<direct::GatewayConnection, Service = GSvc>,
        G: Clone + Send + Sync + Unpin + 'static,
        GSvc: svc::Service<direct::GatewayIo<io::ScopedIo<B::Io>>, Response = ()> + Send + 'static,
        GSvc::Error: Into<Error>,
        GSvc::Future: Send,
        P: profiles::GetProfile<profiles::LookupAddr> + Clone + Send + Sync + Unpin + 'static,
        P::Error: Send,
        P::Future: Send,
    {
        let (listen_addr, listen) = bind
            .bind(&self.config.proxy.server)
            .expect("Failed to bind inbound listener");

        let serve = async move {
            let stack =
                self.to_tcp_connect()
                    .into_server(listen_addr.as_ref().port(), profiles, gateway);
            let shutdown = self.runtime.drain.signaled();
            serve::serve(listen, stack, shutdown).await
        };

        (listen_addr, serve)
    }
}

impl<C> Inbound<C>
where
    C: svc::Service<TcpEndpoint> + Clone + Send + Sync + Unpin + 'static,
    C::Response: io::AsyncRead + io::AsyncWrite + Send + Unpin + 'static,
    C::Error: Into<Error>,
    C::Future: Send,
{
    pub fn push_tcp_forward<I>(
        self,
        server_port: u16,
    ) -> Inbound<
        svc::BoxNewService<
            TcpEndpoint,
            impl svc::Service<I, Response = (), Error = Error, Future = impl Send> + Clone,
        >,
    >
    where
        I: io::AsyncRead + io::AsyncWrite,
        I: Debug + Send + Sync + Unpin + 'static,
    {
        self.map_stack(|_, rt, connect| {
            let prevent_loop = PreventLoop::from(server_port);

            // Forwards TCP streams that cannot be decoded as HTTP.
            //
            // Looping is always prevented.
            connect
                .push_request_filter(prevent_loop)
                .push(rt.metrics.transport.layer_connect())
                .push_make_thunk()
                .push_on_response(
                    svc::layers()
                        .push(tcp::Forward::layer())
                        .push(drain::Retain::layer(rt.drain.clone())),
                )
                .instrument(|_: &_| debug_span!("tcp"))
                .push(svc::BoxNewService::layer())
                .check_new::<TcpEndpoint>()
        })
    }

    pub fn into_server<T, I, G, GSvc, P>(
        self,
        server_port: u16,
        profiles: P,
        gateway: G,
    ) -> svc::BoxNewService<T, svc::BoxService<I, (), Error>>
    where
        T: svc::Param<Remote<ClientAddr>> + svc::Param<OrigDstAddr>,
        T: Clone + Send + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::Peek + io::PeerAddr,
        I: Debug + Send + Sync + Unpin + 'static,
        G: svc::NewService<direct::GatewayConnection, Service = GSvc>,
        G: Clone + Send + Sync + Unpin + 'static,
        GSvc: svc::Service<direct::GatewayIo<I>, Response = ()> + Send + 'static,
        GSvc::Error: Into<Error>,
        GSvc::Future: Send,
        P: profiles::GetProfile<profiles::LookupAddr> + Clone + Send + Sync + Unpin + 'static,
        P::Error: Send,
        P::Future: Send,
    {
        let Self {
            config:
                Config {
                    proxy: config,
                    require_identity_for_inbound_ports: require_id,
                    disable_protocol_detection_for_ports: disable_detect,
                    ..
                },
            runtime: rt,
            stack: _,
        } = self.clone();

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
                    .into_stack()
                    .push_map_target(TcpEndpoint::from)
                    .push_on_response(svc::BoxService::layer())
                    .into_inner(),
            ))
            .push_on_response(svc::BoxService::layer())
            .push_map_target(detect::allow_timeout)
            .push(svc::BoxNewService::layer())
            .push(detect::NewDetectService::layer(
                config.detect_protocol_timeout,
                http::DetectHttp::default(),
            ))
            .push_request_filter(require_id)
            .push(rt.metrics.transport.layer_accept())
            .push_request_filter(TcpAccept::try_from)
            .push(svc::BoxNewService::layer())
            .push(tls::NewDetectTls::layer(
                rt.identity.clone(),
                config.detect_protocol_timeout,
            ))
            .instrument(|_: &_| debug_span!("proxy"))
            .push_switch(
                move |t: T| {
                    let OrigDstAddr(addr) = t.param();
                    if !disable_detect.contains(&addr.port()) {
                        Ok::<_, Never>(svc::Either::A(t))
                    } else {
                        Ok(svc::Either::B(TcpAccept::port_skipped(t)))
                    }
                },
                self.clone()
                    .push_tcp_forward(server_port)
                    .stack
                    .push_map_target(TcpEndpoint::from)
                    .push(rt.metrics.transport.layer_accept())
                    .check_new_service::<TcpAccept, _>()
                    .instrument(|_: &TcpAccept| debug_span!("forward"))
                    .into_inner(),
            )
            .check_new_service::<T, I>()
            .push_on_response(svc::BoxService::layer())
            .push(svc::BoxNewService::layer())
            .push_switch(
                PreventLoop::from(server_port).to_switch(),
                self.push_tcp_forward(server_port)
                    .push_direct(gateway)
                    .stack
                    .instrument(|_: &_| debug_span!("direct"))
                    .into_inner(),
            )
            .instrument(|a: &T| {
                let OrigDstAddr(target_addr) = a.param();
                info_span!("server", port = target_addr.port())
            })
            .push(rt.metrics.tcp_accept_errors.layer())
            .push_on_response(svc::BoxService::layer())
            .push(svc::BoxNewService::layer())
            .into_inner()
    }
}

fn stack_labels(proto: &'static str, name: &'static str) -> metrics::StackLabels {
    metrics::StackLabels::inbound(proto, name)
}
