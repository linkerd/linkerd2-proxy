//! Configures and runs the inbound proxy.
//!
//! The inbound proxy is responsible for terminating traffic from other network
//! endpoints inbound to the local application.

#![deny(warnings, rust_2018_idioms)]

use self::allow_discovery::AllowProfile;
pub use self::endpoint::{
    HttpEndpoint, ProfileTarget, RequestTarget, Target, TcpAccept, TcpEndpoint,
};
use self::prevent_loop::PreventLoop;
use self::require_identity_for_ports::RequireIdentityForPorts;
use futures::future;
use linkerd2_app_core::{
    classify,
    config::{ProxyConfig, ServerConfig},
    drain, dst, errors, metrics,
    opencensus::proto::trace::v1 as oc,
    profiles,
    proxy::{
        http::{self, orig_proto, strip_header},
        identity, tap, tcp,
    },
    reconnect,
    spans::SpanConverter,
    svc::{self},
    transport::{self, io, listen, tls},
    Error, NameAddr, NameMatch, TraceContext, DST_OVERRIDE_HEADER,
};
use std::{collections::HashMap, time::Duration};
use tokio::{net::TcpStream, sync::mpsc};
use tracing::debug_span;

mod allow_discovery;
pub mod endpoint;
mod prevent_loop;
mod require_identity_for_ports;

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

type SensorIo<T> = io::SensorIo<T, transport::metrics::Sensor>;

// === impl Config ===

impl Config {
    pub fn build<L, S, P>(
        self,
        listen_addr: std::net::SocketAddr,
        local_identity: tls::Conditional<identity::Local>,
        http_loopback: L,
        profiles_client: P,
        tap_layer: tap::Layer,
        metrics: metrics::Proxy,
        span_sink: Option<mpsc::Sender<oc::Span>>,
        drain: drain::Watch,
    ) -> impl svc::NewService<
        listen::Addrs,
        Service = impl tower::Service<
            tokio::net::TcpStream,
            Response = (),
            Error = impl Into<Error>,
            Future = impl Send + 'static,
        > + Send
                      + 'static,
    > + Clone
           + Send
           + 'static
    where
        L: svc::NewService<Target, Service = S> + Unpin + Clone + Send + Sync + 'static,
        S: tower::Service<
                http::Request<http::boxed::Payload>,
                Response = http::Response<http::boxed::Payload>,
            > + Unpin
            + Send
            + 'static,
        S::Error: Into<Error>,
        S::Future: Unpin + Send,
        P: profiles::GetProfile<NameAddr> + Unpin + Clone + Send + Sync + 'static,
        P::Future: Unpin + Send,
        P::Error: Send,
    {
        let prevent_loop = PreventLoop::from(listen_addr.port());
        let tcp_connect = self.build_tcp_connect(prevent_loop, &metrics);
        let http_router = self.build_http_router(
            tcp_connect.clone(),
            prevent_loop,
            http_loopback,
            profiles_client,
            tap_layer,
            metrics.clone(),
            span_sink.clone(),
        );

        // Forwards TCP streams that cannot be decoded as HTTP.
        let tcp_forward = svc::stack(tcp_connect)
            .push_make_thunk()
            .push_on_response(svc::layer::mk(tcp::Forward::new))
            .instrument(|_: &_| debug_span!("tcp"))
            .into_inner();

        let accept = self.build_accept(
            tcp_forward.clone(),
            http_router,
            metrics.clone(),
            span_sink,
            drain,
        );

        self.build_tls_accept(accept, tcp_forward, local_identity, metrics)
    }

    pub fn build_tcp_connect(
        &self,
        prevent_loop: PreventLoop,
        metrics: &metrics::Proxy,
    ) -> impl tower::Service<
        TcpEndpoint,
        Error = impl Into<Error>,
        Future = impl future::Future + Unpin + Send,
        Response = impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    > + tower::Service<
        HttpEndpoint,
        Error = impl Into<Error>,
        Future = impl future::Future + Unpin + Send,
        Response = impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    > + Unpin
           + Clone
           + Send {
        // Establishes connections to remote peers (for both TCP
        // forwarding and HTTP proxying).
        svc::connect(self.proxy.connect.keepalive)
            .push_map_response(io::BoxedIo::new) // Ensures the transport propagates shutdown properly.
            // Limits the time we wait for a connection to be established.
            .push_timeout(self.proxy.connect.timeout)
            .push(metrics.transport.layer_connect())
            .push_request_filter(prevent_loop)
            .into_inner()
    }

    pub fn build_http_router<C, P, L, S>(
        &self,
        tcp_connect: C,
        prevent_loop: impl Into<PreventLoop>,
        loopback: L,
        profiles_client: P,
        tap_layer: tap::Layer,
        metrics: metrics::Proxy,
        span_sink: Option<mpsc::Sender<oc::Span>>,
    ) -> impl svc::NewService<
        Target,
        Service = impl tower::Service<
            http::Request<http::boxed::Payload>,
            Response = http::Response<http::boxed::Payload>,
            Error = Error,
            Future = impl Send,
        > + Unpin
                      + Send,
    > + Unpin
           + Clone
           + Send
    where
        C: tower::Service<HttpEndpoint> + Unpin + Clone + Send + Sync + 'static,
        C::Error: Into<Error>,
        C::Response: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
        C::Future: Unpin + Send,
        P: profiles::GetProfile<NameAddr> + Unpin + Clone + Send + Sync + 'static,
        P::Future: Unpin + Send,
        P::Error: Send,
        // The loopback router processes requests sent to the inbound port.
        L: svc::NewService<Target, Service = S> + Unpin + Send + Clone + Sync + 'static,
        S: tower::Service<
                http::Request<http::boxed::Payload>,
                Response = http::Response<http::boxed::Payload>,
            > + Unpin
            + Send
            + 'static,
        S::Error: Into<Error>,
        S::Future: Unpin + Send,
    {
        let Config {
            allow_discovery,
            proxy:
                ProxyConfig {
                    connect,
                    buffer_capacity,
                    cache_max_idle_age,
                    dispatch_timeout,
                    ..
                },
            ..
        } = self.clone();

        let prevent_loop = prevent_loop.into();

        // Creates HTTP clients for each inbound port & HTTP settings.
        let endpoint = svc::stack(tcp_connect)
            .push(http::client::layer(
                connect.h1_settings,
                connect.h2_settings,
            ))
            .push(reconnect::layer({
                let backoff = connect.backoff;
                move |_| Ok(backoff.stream())
            }))
            .check_new_service::<HttpEndpoint, http::Request<_>>();

        let observe = svc::layers()
            // Registers the stack to be tapped.
            .push(tap_layer)
            // Records metrics for each `Target`.
            .push(metrics.http_endpoint.into_layer::<classify::Response>())
            .push_on_response(TraceContext::layer(
                span_sink
                    .clone()
                    .map(|span_sink| SpanConverter::client(span_sink, trace_labels())),
            ));

        let target = endpoint
            .push_map_target(HttpEndpoint::from)
            .push(observe)
            .push_on_response(svc::layers().box_http_response())
            .check_new_service::<Target, http::Request<_>>();

        // Attempts to discover a service profile for each logical target (as
        // informed by the request's headers). The stack is cached until a
        // request has not been received for `cache_max_idle_age`.
        let profile = target
            .clone()
            .check_new_service::<Target, http::Request<http::boxed::Payload>>()
            .push_on_response(svc::layers().box_http_request())
            // The target stack doesn't use the profile resolution, so drop it.
            .push_map_target(endpoint::Target::from)
            .push(profiles::http::route_request::layer(
                svc::proxies()
                    // Sets the route as a request extension so that it can be used
                    // by tap.
                    .push_http_insert_target()
                    // Records per-route metrics.
                    .push(metrics.http_route.into_layer::<classify::Response>())
                    // Sets the per-route response classifier as a request
                    // extension.
                    .push(classify::Layer::new())
                    .check_new_clone::<dst::Route>()
                    .push_map_target(endpoint::route)
                    .into_inner(),
            ))
            .push_map_target(endpoint::Logical::from)
            .push(profiles::discover::layer(
                profiles_client,
                AllowProfile(allow_discovery),
            ))
            .push_on_response(svc::layers().box_http_response())
            .instrument(|_: &Target| debug_span!("profile"))
            // Skip the profile stack if it takes too long to become ready.
            .push_when_unready(target.clone(), self.profile_idle_timeout)
            .check_new_service::<Target, http::Request<http::boxed::Payload>>();

        // If the traffic is targeted at the inbound port, send it through
        // the loopback service (i.e. as a gateway).
        let switch_loopback = svc::stack(loopback).push_switch(prevent_loop, profile);

        // Attempts to resolve the target as a service profile or, if that
        // fails, skips that stack to forward to the local endpoint.
        svc::stack(switch_loopback)
            .check_new_service::<Target, http::Request<http::boxed::Payload>>()
            .cache(
                svc::layers().push_on_response(
                    svc::layers()
                        .push_failfast(dispatch_timeout)
                        .push_spawn_buffer_with_idle_timeout(buffer_capacity, cache_max_idle_age)
                        .push(metrics.stack.layer(stack_labels("logical")))
                        .box_http_response(),
                ),
            )
            // Boxing is necessary purely to limit the link-time overhead of
            // having enormous types.
            .box_new_service()
            .check_new_service::<Target, http::Request<http::boxed::Payload>>()
            .into_inner()
    }

    pub fn build_accept<I, F, A, H, S>(
        &self,
        tcp_forward: F,
        http_router: H,
        metrics: metrics::Proxy,
        span_sink: Option<mpsc::Sender<oc::Span>>,
        drain: drain::Watch,
    ) -> impl svc::NewService<
        TcpAccept,
        Service = impl tower::Service<
            I,
            Response = (),
            Error = impl Into<Error>,
            Future = impl Send + 'static,
        > + Send
                      + 'static,
    > + Clone
           + Send
           + 'static
    where
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + Unpin + Send + 'static,
        F: svc::NewService<TcpEndpoint, Service = A> + Unpin + Clone + Send + 'static,
        A: tower::Service<io::PrefixedIo<I>, Response = ()> + Clone + Send + 'static,
        A::Error: Into<Error>,
        A::Future: Send,
        H: svc::NewService<Target, Service = S> + Unpin + Clone + Send + 'static,
        S: tower::Service<
                http::Request<http::boxed::Payload>,
                Response = http::Response<http::boxed::Payload>,
                Error = Error,
            > + Send
            + 'static,
        S::Future: Send,
    {
        let ProxyConfig {
            server: ServerConfig { h2_settings, .. },
            dispatch_timeout,
            max_in_flight_requests,
            detect_protocol_timeout,
            buffer_capacity,
            ..
        } = self.proxy.clone();

        // Handles requests as they are initially received by the proxy.
        let http_admit_request = svc::layers()
            // Downgrades the protocol if upgraded by an outbound proxy.
            .push(svc::layer::mk(orig_proto::Downgrade::new))
            // Limits the number of in-flight requests.
            .push_concurrency_limit(max_in_flight_requests)
            // Eagerly fail requests when the proxy is out of capacity for a
            // dispatch_timeout.
            .push_failfast(dispatch_timeout)
            .push(metrics.http_errors)
            // Synthesizes responses for proxy errors.
            .push(errors::layer());

        let http_server = svc::stack(http_router)
            // Removes the override header after it has been used to
            // determine a reuquest target.
            .push_on_response(strip_header::request::layer(DST_OVERRIDE_HEADER))
            // Routes each request to a target, obtains a service for that
            // target, and dispatches the request.
            .instrument_from_target()
            .push(svc::layer::mk(|inner| {
                svc::stack::NewRouter::new(RequestTarget::from, inner)
            }))
            // Used by tap.
            .push_http_insert_target()
            .check_new_service::<TcpAccept, http::Request<_>>()
            .push(http::normalize_uri::NewNormalizeUri::layer())
            .push_on_response(
                svc::layers()
                    .push(http_admit_request)
                    .push(http::normalize_uri::MarkAbsoluteForm::layer())
                    .push(TraceContext::layer(span_sink.map(|span_sink| {
                        SpanConverter::server(span_sink, trace_labels())
                    })))
                    .push(metrics.stack.layer(stack_labels("source")))
                    .box_http_request()
                    .box_http_response(),
            )
            .push_map_target(|(_, accept): (_, TcpAccept)| accept)
            .instrument(|(v, _): &(http::Version, _)| debug_span!("http", %v))
            .check_new_service::<(http::Version, TcpAccept), http::Request<_>>()
            .into_inner();

        svc::stack(http::DetectHttp::new(
            h2_settings,
            http_server,
            svc::stack(tcp_forward)
                .push_map_target(TcpEndpoint::from)
                .into_inner(),
            drain.clone(),
        ))
        .push_on_response(svc::layers().push_spawn_buffer(buffer_capacity).push(
            transport::Prefix::layer(
                http::Version::DETECT_BUFFER_CAPACITY,
                detect_protocol_timeout,
            ),
        ))
        .into_inner()
    }

    pub fn build_tls_accept<D, A, F, B>(
        self,
        detect: D,
        tcp_forward: F,
        identity: tls::Conditional<identity::Local>,
        metrics: metrics::Proxy,
    ) -> impl svc::NewService<
        listen::Addrs,
        Service = impl tower::Service<
            TcpStream,
            Response = (),
            Error = impl Into<Error>,
            Future = impl Send + 'static,
        > + Send
                      + 'static,
    > + Clone
           + Send
           + 'static
    where
        D: svc::NewService<TcpAccept, Service = A> + Unpin + Clone + Send + Sync + 'static,
        A: tower::Service<SensorIo<io::BoxedIo>, Response = ()> + Unpin + Send + 'static,
        A::Error: Into<Error>,
        A::Future: Send,
        F: svc::NewService<TcpEndpoint, Service = B> + Unpin + Clone + Send + Sync + 'static,
        B: tower::Service<SensorIo<TcpStream>, Response = ()> + Unpin + Send + 'static,
        B::Error: Into<Error>,
        B::Future: Send,
    {
        let ProxyConfig {
            detect_protocol_timeout,
            ..
        } = self.proxy;
        let require_identity = self.require_identity_for_inbound_ports;

        svc::stack(detect)
            .push_request_filter(require_identity)
            .push(metrics.transport.layer_accept())
            .push_map_target(TcpAccept::from)
            .push(tls::DetectTls::layer(
                identity.clone(),
                detect_protocol_timeout,
            ))
            .push_switch(
                self.disable_protocol_detection_for_ports,
                svc::stack(tcp_forward)
                    .push_map_target(TcpEndpoint::from)
                    .push(metrics.transport.layer_accept())
                    .push_map_target(TcpAccept::from)
                    .into_inner(),
            )
            .into_inner()
    }
}

pub fn trace_labels() -> HashMap<String, String> {
    let mut l = HashMap::new();
    l.insert("direction".to_string(), "inbound".to_string());
    l
}

fn stack_labels(name: &'static str) -> metrics::StackLabels {
    metrics::StackLabels::inbound(name)
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
