//! Configures and runs the inbound proxy.
//!
//! The inbound proxy is responsible for terminating traffic from other network
//! endpoints inbound to the local application.

#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]

mod allow_discovery;
pub mod direct;
pub mod http;
mod require_identity;
pub mod target;
#[cfg(any(test, fuzzing))]
pub(crate) mod test_util;

pub use self::target::{HttpEndpoint, Logical, RequestTarget, Target, TcpEndpoint};
use self::{
    require_identity::RequireIdentityForPorts,
    target::{HttpAccept, TcpAccept},
};
use linkerd_app_core::{
    config::{ConnectConfig, PortSet, ProxyConfig, ServerConfig},
    detect, drain, io, metrics, profiles,
    proxy::{identity::LocalCrtKey, tcp},
    serve,
    svc::{self, ExtractParam, InsertParam},
    tls,
    transport::{self, listen::Bind, ClientAddr, Local, OrigDstAddr, Remote, ServerAddr},
    Error, Infallible, NameMatch, ProxyRuntime,
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

#[derive(Clone)]
struct TlsParams {
    timeout: tls::server::Timeout,
    identity: Option<LocalCrtKey>,
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
        let (Local(ServerAddr(la)), listen) = bind
            .bind(&self.config.proxy.server)
            .expect("Failed to bind inbound listener");

        let serve = async move {
            let shutdown = self.runtime.drain.clone().signaled();
            let stack = self
                .into_tcp_connect(la.port())
                .push_server(la.port(), profiles, gateway)
                .into_inner();
            serve::serve(listen, stack, shutdown).await
        };

        (Local(ServerAddr(la)), serve)
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
            // Forwards TCP streams that cannot be decoded as HTTP.
            //
            // Looping is always prevented.
            connect
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

    pub fn push_server<T, I, G, GSvc, P>(
        self,
        proxy_port: u16,
        profiles: P,
        gateway: G,
    ) -> Inbound<svc::BoxNewService<T, svc::BoxService<I, (), Error>>>
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
        // Handles inbound connections that target an opaque port.
        let opaque = self
            .clone()
            .push_tcp_forward()
            .map_stack(|_, rt, tcp| {
                tcp.push_map_target(TcpEndpoint::from)
                    .push(rt.metrics.transport.layer_accept())
                    .check_new_service::<TcpAccept, _>()
            })
            .into_stack();

        // Handles inbound connections that could not be detected as HTTP.
        let tcp = self.clone().push_tcp_forward();

        // Handles connections targeting the inbound proxy port--either by acting as a gateway to
        // the outbound stack or by forwarding connections locally (for opauque transport).
        let direct = tcp
            .clone()
            .push_direct(gateway)
            .into_stack()
            .instrument(|_: &_| debug_span!("direct"));

        self.push_http_router(profiles)
            .push_http_server()
            .map_stack(|cfg, rt, http| {
                let detect_timeout = cfg.proxy.detect_protocol_timeout;
                let require_id = cfg.require_identity_for_inbound_ports.clone();

                http.push_map_target(HttpAccept::from)
                    .push(svc::UnwrapOr::layer(
                        // When HTTP detection fails, forward the connection to the application as
                        // an opaque TCP stream.
                        tcp.into_stack()
                            .push_map_target(TcpEndpoint::from)
                            .push_on_response(svc::BoxService::layer())
                            .into_inner(),
                    ))
                    .push_map_target(detect::allow_timeout)
                    .push(svc::BoxNewService::layer())
                    .push(detect::NewDetectService::layer(detect::Config::<
                        http::DetectHttp,
                    >::from_timeout(
                        detect_timeout
                    )))
                    .check_new_service::<TcpAccept, _>()
                    .push_request_filter(require_id)
                    .push(rt.metrics.transport.layer_accept())
                    .push_request_filter(TcpAccept::try_from)
                    .push(svc::BoxNewService::layer())
                    .push(tls::NewDetectTls::layer(TlsParams {
                        timeout: tls::server::Timeout(detect_timeout),
                        identity: rt.identity.clone(),
                    }))
            })
            .map_stack(|cfg, rt, detect| {
                let disable_detect = cfg.disable_protocol_detection_for_ports.clone();
                detect
                    .instrument(|_: &_| debug_span!("proxy"))
                    .push_switch(
                        // If the connection targets a port on which protocol detection is disabled,
                        // then we forward it directly to the application, bypassing protocol
                        // detection.
                        move |t: T| -> Result<_, Infallible> {
                            let OrigDstAddr(addr) = t.param();
                            if disable_detect.contains(&addr.port()) {
                                return Ok(svc::Either::B(TcpAccept::port_skipped(t)));
                            }
                            Ok(svc::Either::A(t))
                        },
                        opaque
                            .instrument(|_: &TcpAccept| debug_span!("forward"))
                            .into_inner(),
                    )
                    .push_switch(
                        // If the connection targets the inbound proxy port, the connection is most
                        // likely using opaque transport to target an alternate port, or possibly an
                        // outbound target if the proxy is configured as a gateway. The direct stack
                        // handles these connections.
                        move |t: T| -> Result<_, Infallible> {
                            let OrigDstAddr(a) = t.param();
                            if a.port() == proxy_port {
                                return Ok(svc::Either::B(t));
                            }
                            Ok(svc::Either::A(t))
                        },
                        direct.into_inner(),
                    )
                    .instrument(|a: &T| {
                        let OrigDstAddr(target_addr) = a.param();
                        info_span!("server", port = target_addr.port())
                    })
                    .push(rt.metrics.tcp_accept_errors.layer())
                    .push_on_response(svc::BoxService::layer())
                    .push(svc::BoxNewService::layer())
            })
    }
}

fn stack_labels(proto: &'static str, name: &'static str) -> metrics::StackLabels {
    metrics::StackLabels::inbound(proto, name)
}

// === TlsParams ===

impl<T> ExtractParam<tls::server::Timeout, T> for TlsParams {
    #[inline]
    fn extract_param(&self, _: &T) -> tls::server::Timeout {
        self.timeout
    }
}

impl<T> ExtractParam<Option<LocalCrtKey>, T> for TlsParams {
    #[inline]
    fn extract_param(&self, _: &T) -> Option<LocalCrtKey> {
        self.identity.clone()
    }
}

impl<T> InsertParam<tls::ConditionalServerTls, T> for TlsParams {
    type Target = (tls::ConditionalServerTls, T);

    #[inline]
    fn insert_param(&self, tls: tls::ConditionalServerTls, target: T) -> Self::Target {
        (tls, target)
    }
}
