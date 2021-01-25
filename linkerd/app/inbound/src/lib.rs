//! Configures and runs the inbound proxy.
//!
//! The inbound proxy is responsible for terminating traffic from other network
//! endpoints inbound to the local application.

#![deny(warnings, rust_2018_idioms)]

mod allow_discovery;
mod direct;
mod http;
mod prevent_loop;
mod require_identity;
pub mod target;

pub use self::target::{HttpEndpoint, Logical, RequestTarget, Target, TcpAccept, TcpEndpoint};
use self::{prevent_loop::PreventLoop, require_identity::RequireIdentityForPorts};
use linkerd_app_core::{
    config::{ConnectConfig, ProxyConfig},
    detect, drain, io, metrics,
    opencensus::proto::trace::v1 as oc,
    profiles,
    proxy::{identity::LocalCrtKey, tap, tcp},
    svc, tls,
    transport::{self, listen},
    Error, NameAddr, NameMatch,
};
use std::{fmt::Debug, net::SocketAddr, time::Duration};
use tokio::sync::mpsc;
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

// === impl Config ===

pub fn tcp_connect<T: Into<u16>>(
    config: &ConnectConfig,
) -> impl svc::Service<
    T,
    Response = impl io::AsyncRead + io::AsyncWrite + Send,
    Error = Error,
    Future = impl Send,
> + Clone {
    // Establishes connections to remote peers (for both TCP
    // forwarding and HTTP proxying).
    svc::stack(transport::ConnectTcp::new(config.keepalive))
        .push_map_target(|t: T| ([127, 0, 0, 1], t.into()))
        // Limits the time we wait for a connection to be established.
        .push_timeout(config.timeout)
        .push(svc::stack::BoxFuture::layer())
        .into_inner()
}

#[allow(clippy::too_many_arguments)]
impl Config {
    pub fn build<I, C, L, LSvc, P>(
        self,
        listen_addr: SocketAddr,
        local_identity: Option<LocalCrtKey>,
        connect: C,
        http_loopback: L,
        profiles_client: P,
        tap: tap::Registry,
        metrics: metrics::Proxy,
        span_sink: Option<mpsc::Sender<oc::Span>>,
        drain: drain::Watch,
    ) -> impl svc::NewService<
        listen::Addrs,
        Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send>,
    > + Clone
    where
        I: io::AsyncRead + io::AsyncWrite + io::Peek + io::PeerAddr,
        I: Debug + Send + Sync + Unpin + 'static,
        C: svc::Service<TcpEndpoint> + Clone + Send + Sync + Unpin + 'static,
        C::Response: io::AsyncRead + io::AsyncWrite + Send + Unpin + 'static,
        C::Error: Into<Error>,
        C::Future: Send + Unpin,
        L: svc::NewService<Target, Service = LSvc> + Clone + Send + Sync + Unpin + 'static,
        LSvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>
            + Send
            + 'static,
        LSvc::Error: Into<Error>,
        LSvc::Future: Send,
        P: profiles::GetProfile<NameAddr> + Clone + Send + Sync + 'static,
        P::Error: Send,
        P::Future: Send,
    {
        let prevent_loop = PreventLoop::from(listen_addr.port());

        // Forwards TCP streams that cannot be decoded as HTTP.
        //
        // Looping is always prevented.
        let tcp_forward = svc::stack(connect.clone())
            .push_request_filter(prevent_loop)
            .push(metrics.transport.layer_connect())
            .push_make_thunk()
            .push_on_response(
                svc::layers()
                    .push(tcp::Forward::layer())
                    .push(drain::Retain::layer(drain.clone())),
            )
            .instrument(|_: &_| debug_span!("tcp"))
            .check_new::<TcpEndpoint>();

        let direct = direct::stack(
            &self.proxy,
            local_identity.clone(),
            tcp_forward.clone().into_inner(),
            http_loopback,
            &metrics,
            span_sink.clone(),
            drain.clone(),
        );

        let http = http::router(
            &self,
            connect,
            profiles_client,
            tap,
            &metrics,
            span_sink.clone(),
        );
        svc::stack(http::server(&self.proxy, http, &metrics, span_sink, drain))
            .push(svc::NewUnwrapOr::layer(
                // When HTTP detection fails, forward the connection to the
                // application as an opaque TCP stream.
                tcp_forward
                    .clone()
                    .push_map_target(TcpEndpoint::from)
                    .into_inner(),
            ))
            .push_cache(self.proxy.cache_max_idle_age)
            .push(detect::NewDetectService::timeout(
                self.proxy.detect_protocol_timeout,
                http::DetectHttp::default(),
            ))
            .push_request_filter(self.require_identity_for_inbound_ports)
            .push(metrics.transport.layer_accept())
            .push_map_target(TcpAccept::from)
            .push(tls::NewDetectTls::layer(
                local_identity,
                self.proxy.detect_protocol_timeout,
            ))
            .push_switch(
                self.disable_protocol_detection_for_ports,
                tcp_forward
                    .push_map_target(TcpEndpoint::from)
                    .push(metrics.transport.layer_accept())
                    .push_map_target(TcpAccept::port_skipped)
                    .into_inner(),
            )
            .push_switch(prevent_loop, direct)
            .into_inner()
    }
}

// === impl SkipByPort ===

impl From<indexmap::IndexSet<u16>> for SkipByPort {
    fn from(ports: indexmap::IndexSet<u16>) -> Self {
        SkipByPort(ports.into())
    }
}

impl svc::stack::Switch<listen::Addrs> for SkipByPort {
    fn use_primary(&self, t: &listen::Addrs) -> bool {
        !self.0.contains(&t.target_addr().port())
    }
}
