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
    serve,
    svc::{self, Param},
    tls,
    transport::{
        self, listen::DefaultOrigDstAddr, ClientAddr, Local, OrigDstAddr, Remote, ServerAddr,
    },
    Error, NameMatch, ProxyRuntime,
};
use std::{convert::TryFrom, fmt::Debug, future::Future, net::SocketAddr, time::Duration};
use tracing::{debug_span, info_span};

#[derive(Clone, Debug)]
pub struct Config<A = DefaultOrigDstAddr> {
    pub allow_discovery: NameMatch,
    pub proxy: ProxyConfig<A>,
    pub require_identity_for_inbound_ports: RequireIdentityForPorts,
    pub disable_protocol_detection_for_ports: SkipByPort,
    pub profile_idle_timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct SkipByPort(std::sync::Arc<indexmap::IndexSet<u16>>);

#[derive(Clone, Debug)]
pub struct Inbound<S, A> {
    config: Config<A>,
    runtime: ProxyRuntime,
    stack: svc::Stack<S>,
}

// === impl Config ===

impl<A> Config<A> {
    pub fn with_orig_dst<A2>(self, orig_dst: A2) -> Config<A2> {
        Config {
            allow_discovery: self.allow_discovery,
            proxy: self.proxy.with_orig_dst_addr(orig_dst),
            require_identity_for_inbound_ports: self.require_identity_for_inbound_ports,
            disable_protocol_detection_for_ports: self.disable_protocol_detection_for_ports,
            profile_idle_timeout: self.profile_idle_timeout,
        }
    }
}

// === impl Inbound ===

impl<S, A> Inbound<S, A> {
    pub fn config(&self) -> &Config<A> {
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
}

impl<A> Inbound<(), A> {
    pub fn new(config: Config<A>, runtime: ProxyRuntime) -> Self {
        Self {
            config,
            runtime,
            stack: svc::stack(()),
        }
    }

    pub fn with_stack<S>(self, stack: S) -> Inbound<S, A> {
        Inbound {
            config: self.config,
            runtime: self.runtime,
            stack: svc::stack(stack),
        }
    }

    pub fn to_tcp_connect<T: Param<u16>>(
        &self,
    ) -> Inbound<
        impl svc::Service<
                T,
                Response = impl io::AsyncRead + io::AsyncWrite + Send,
                Error = Error,
                Future = impl Send,
            > + Clone,
        A,
    >
    where
        A: Clone,
    {
        let Self {
            config, runtime, ..
        } = self.clone();

        // Establishes connections to remote peers (for both TCP
        // forwarding and HTTP proxying).
        let ConnectConfig {
            keepalive, timeout, ..
        } = config.proxy.connect;

        let stack = svc::stack(transport::ConnectTcp::new(keepalive))
            .push_map_target(|t: T| Remote(ServerAddr(([127, 0, 0, 1], t.param()).into())))
            // Limits the time we wait for a connection to be established.
            .push_timeout(timeout)
            .push(svc::stack::BoxFuture::layer());

        Inbound {
            config,
            runtime,
            stack,
        }
    }

    pub fn serve<G, GSvc, P>(
        self,
        profiles: P,
        gateway: G,
    ) -> (SocketAddr, impl Future<Output = ()> + Send)
    where
        A: transport::GetAddrs<io::ScopedIo<tokio::net::TcpStream>> + Send + 'static,
        A::Addrs: Param<Remote<ClientAddr>>
            + Param<Local<ServerAddr>>
            + Param<OrigDstAddr>
            + Clone
            + Send,
        G: svc::NewService<direct::GatewayConnection, Service = GSvc>,
        G: Clone + Send + Sync + Unpin + 'static,
        GSvc: svc::Service<direct::GatewayIo<io::ScopedIo<tokio::net::TcpStream>>, Response = ()>
            + Send
            + 'static,
        GSvc::Error: Into<Error>,
        GSvc::Future: Send,
        P: profiles::GetProfile<profiles::LookupAddr> + Clone + Send + Sync + 'static,
        P::Error: Send,
        P::Future: Send,
    {
        let (listen_addr, listen) = self
            .config
            .proxy
            .server
            .bind
            .bind()
            .expect("Failed to bind inbound listener");

        let serve = async move {
            let stack = self
                .to_tcp_connect()
                .into_server(listen_addr.port(), profiles, gateway);
            let shutdown = self.runtime.drain.signaled();
            serve::serve(listen, stack, shutdown).await
        };

        (listen_addr, serve)
    }
}

impl<C, A> Inbound<C, A>
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
        A,
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
    ) -> impl for<'a> svc::NewService<
        &'a I,
        Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send>,
    > + Clone
    where
        A: transport::GetAddrs<I> + 'static,
        A::Addrs: Param<Remote<ClientAddr>> + Param<OrigDstAddr> + Clone + Send + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::Peek + io::PeerAddr,
        I: Debug + Send + Sync + Unpin + 'static,
        G: svc::NewService<direct::GatewayConnection, Service = GSvc>,
        G: Clone + Send + Sync + Unpin + 'static,
        GSvc: svc::Service<direct::GatewayIo<I>, Response = ()> + Send + 'static,
        GSvc::Error: Into<Error>,
        GSvc::Future: Send,
        P: profiles::GetProfile<profiles::LookupAddr> + Clone + Send + Sync + 'static,
        P::Error: Send,
        P::Future: Send,
    {
        let disable_detect = self.config.disable_protocol_detection_for_ports.clone();
        let require_id = self.config.require_identity_for_inbound_ports.clone();
        let config = self.config.proxy.clone();
        let addrs = self.config.proxy.server.orig_dst_addrs.clone();
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
            .push_map_target(detect::allow_timeout)
            .push(detect::NewDetectService::layer(
                config.detect_protocol_timeout,
                http::DetectHttp::default(),
            ))
            .push_request_filter(require_id)
            .push(self.runtime.metrics.transport.layer_accept())
            .push_request_filter(TcpAccept::try_from)
            .push(tls::NewDetectTls::layer(
                self.runtime.identity.clone(),
                config.detect_protocol_timeout,
            ))
            .instrument(|_: &_| debug_span!("proxy"))
            .push_switch(
                disable_detect,
                self.clone()
                    .push_tcp_forward(server_port)
                    .stack
                    .push_map_target(TcpEndpoint::from)
                    .push(self.runtime.metrics.transport.layer_accept())
                    .push_map_target(TcpAccept::port_skipped)
                    .check_new_service::<A::Addrs, _>()
                    .instrument(|_: &A::Addrs| debug_span!("forward"))
                    .into_inner(),
            )
            .check_new_service::<A::Addrs, I>()
            .push_switch(
                PreventLoop::from(server_port).to_switch(),
                self.push_tcp_forward(server_port)
                    .push_direct(gateway)
                    .stack
                    .instrument(|_: &_| debug_span!("direct"))
                    .into_inner(),
            )
            .instrument(|a: &A::Addrs| {
                let OrigDstAddr(target_addr) = a.param();
                info_span!("server", port = target_addr.port())
            })
            .check_new_service::<A::Addrs, I>()
            .push_request_filter(transport::AddrsFilter(addrs))
            .into_inner()
    }
}

// === impl SkipByPort ===

impl From<indexmap::IndexSet<u16>> for SkipByPort {
    fn from(ports: indexmap::IndexSet<u16>) -> Self {
        SkipByPort(ports.into())
    }
}

impl<T> svc::Predicate<T> for SkipByPort
where
    T: Param<OrigDstAddr>,
{
    type Request = svc::Either<T, T>;

    fn check(&mut self, t: T) -> Result<Self::Request, Error> {
        let OrigDstAddr(addr) = t.param();
        if !self.0.contains(&addr.port()) {
            Ok(svc::Either::A(t))
        } else {
            Ok(svc::Either::B(t))
        }
    }
}

fn stack_labels(proto: &'static str, name: &'static str) -> metrics::StackLabels {
    metrics::StackLabels::inbound(proto, name)
}
