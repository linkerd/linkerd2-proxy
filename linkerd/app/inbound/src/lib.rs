//! Configures and runs the inbound proxy.
//!
//! The inbound proxy is responsible for terminating traffic from other network
//! endpoints inbound to the local application.

#![deny(warnings, rust_2018_idioms)]

pub use self::endpoint::{
    HttpEndpoint, Profile, ProfileTarget, RequestTarget, Target, TcpEndpoint,
};
use self::require_identity_for_ports::RequireIdentityForPorts;
use futures::future;
use linkerd2_app_core::{
    admit, classify,
    config::{ProxyConfig, ServerConfig},
    drain, dst, errors, metric_labels,
    opencensus::proto::trace::v1 as oc,
    profiles,
    proxy::{
        core::Listen,
        detect::DetectProtocolLayer,
        http::{self, normalize_uri, orig_proto, strip_header},
        identity,
        server::{Protocol as ServerProtocol, ProtocolDetect, Server},
        tap, tcp,
    },
    reconnect, router, serve,
    spans::SpanConverter,
    svc::{self, NewService},
    transport::{self, io::BoxedIo, tls},
    Error, ProxyMetrics, TraceContextLayer, DST_OVERRIDE_HEADER, L5D_CLIENT_ID, L5D_REMOTE_IP,
    L5D_SERVER_ID,
};
use std::collections::HashMap;
use tokio::sync::mpsc;
use tracing::{info, info_span};

pub mod endpoint;
mod prevent_loop;
mod require_identity_for_ports;
#[allow(dead_code)] // TODO #2597
mod set_client_id_on_req;
#[allow(dead_code)] // TODO #2597
mod set_remote_ip_on_req;

use self::prevent_loop::PreventLoop;

#[derive(Clone, Debug)]
pub struct Config {
    pub proxy: ProxyConfig,
    pub require_identity_for_inbound_ports: RequireIdentityForPorts,
}

impl Config {
    pub fn build<L, S, P>(
        self,
        listen: transport::Listen<transport::DefaultOrigDstAddr>,
        local_identity: tls::Conditional<identity::Local>,
        http_loopback: L,
        profiles_client: P,
        tap_layer: tap::Layer,
        metrics: ProxyMetrics,
        span_sink: Option<mpsc::Sender<oc::Span>>,
        drain: drain::Watch,
    ) -> serve::Task
    where
        L: tower::Service<Target, Response = S> + Send + Clone + 'static,
        L::Error: Into<Error>,
        L::Future: Send,
        S: tower::Service<
                http::Request<http::boxed::Payload>,
                Response = http::Response<http::boxed::Payload>,
            > + Send
            + 'static,
        S::Error: Into<Error>,
        S::Future: Send,
        P: profiles::GetRoutes<Profile> + Clone + Send + 'static,
        P::Future: Send,
    {
        let tcp_connect = self.build_tcp_connect(&metrics);
        let prevent_loop = PreventLoop::from(listen.listen_addr().port());
        let http_router = self.build_http_router(
            tcp_connect.clone(),
            prevent_loop,
            http_loopback,
            profiles_client,
            tap_layer,
            metrics.clone(),
            span_sink.clone(),
        );
        self.build_server(
            listen,
            prevent_loop,
            tcp_connect,
            http_router,
            local_identity,
            metrics,
            span_sink,
            drain,
        )
    }

    pub fn build_tcp_connect(
        &self,
        metrics: &ProxyMetrics,
    ) -> impl tower::Service<
        TcpEndpoint,
        Error = impl Into<Error>,
        Future = impl future::Future + Send,
        Response = impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
    > + tower::Service<
        HttpEndpoint,
        Error = impl Into<Error>,
        Future = impl future::Future + Send,
        Response = impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
    > + Clone
           + Send {
        // Establishes connections to remote peers (for both TCP
        // forwarding and HTTP proxying).
        svc::connect(self.proxy.connect.keepalive)
            .push_map_response(BoxedIo::new) // Ensures the transport propagates shutdown properly.
            // Limits the time we wait for a connection to be established.
            .push_timeout(self.proxy.connect.timeout)
            .push(metrics.transport.layer_connect(TransportLabels))
            .into_inner()
    }

    pub fn build_http_router<C, P, L, S>(
        &self,
        tcp_connect: C,
        prevent_loop: impl Into<PreventLoop>,
        http_loopback: L,
        profiles_client: P,
        tap_layer: tap::Layer,
        metrics: ProxyMetrics,
        span_sink: Option<mpsc::Sender<oc::Span>>,
    ) -> impl tower::Service<
        Target,
        Error = impl Into<Error>,
        Future = impl Send,
        Response = impl tower::Service<
            http::Request<http::boxed::Payload>,
            Response = http::Response<http::boxed::Payload>,
            Error = impl Into<Error>,
            Future = impl Send,
        > + Send,
    > + Clone
           + Send
    where
        C: tower::Service<HttpEndpoint> + Clone + Send + Sync + 'static,
        C::Error: Into<Error>,
        C::Response: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
        C::Future: Send,
        P: profiles::GetRoutes<Profile> + Clone + Send + 'static,
        P::Future: Send,
        // The loopback router processes requests sent to the inbound port.
        L: tower::Service<Target, Response = S> + Send + Clone + 'static,
        L::Error: Into<Error>,
        L::Future: Send,
        S: tower::Service<
                http::Request<http::boxed::Payload>,
                Response = http::Response<http::boxed::Payload>,
            > + Send
            + 'static,
        S::Error: Into<Error>,
        S::Future: Send,
    {
        let Config {
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
        let http_endpoint = svc::stack(tcp_connect)
            .push(http::MakeClientLayer::new(connect.h2_settings))
            .push(reconnect::layer({
                let backoff = connect.backoff.clone();
                move |_| Ok(backoff.stream())
            }))
            .check_service::<HttpEndpoint>();

        let http_target_observability = svc::layers()
            // Registers the stack to be tapped.
            .push(tap_layer)
            // Records metrics for each `Target`.
            .push(metrics.http_endpoint.into_layer::<classify::Response>())
            .push_on_response(TraceContextLayer::new(
                span_sink
                    .clone()
                    .map(|span_sink| SpanConverter::client(span_sink, trace_labels())),
            ));

        let http_profile_route_proxy = svc::proxies()
            // Sets the route as a request extension so that it can be used
            // by tap.
            .push_http_insert_target()
            // Records per-route metrics.
            .push(metrics.http_route.into_layer::<classify::Response>())
            // Sets the per-route response classifier as a request
            // extension.
            .push(classify::Layer::new())
            .check_new_clone_service::<dst::Route>();

        // An HTTP client is created for each target via the endpoint stack.
        let http_target_cache = http_endpoint
            .push_map_target(HttpEndpoint::from)
            // Normalizes the URI, i.e. if it was originally in
            // absolute-form on the outbound side.
            .push(normalize_uri::layer())
            .push(http_target_observability)
            .check_service::<Target>()
            .into_new_service()
            .cache(
                svc::layers().push_on_response(
                    svc::layers()
                        // If the service has been unavailable for an extended time, eagerly
                        // fail requests.
                        .push_failfast(dispatch_timeout)
                        // Shares the service, ensuring discovery errors are propagated.
                        .push_spawn_buffer_with_idle_timeout(buffer_capacity, cache_max_idle_age)
                        .push(metrics.stack.layer(stack_labels("target"))),
                ),
            )
            .instrument(|_: &Target| info_span!("target"))
            // Prevent the cache's lock from being acquired in poll_ready, ensuring this happens
            // in the response future. This prevents buffers from holding the cache's lock.
            .push_oneshot()
            .check_service::<Target>();

        // Routes targets to a Profile stack, i.e. so that profile
        // resolutions are shared even as the type of request may vary.
        let http_profile_cache = http_target_cache
            .clone()
            .push_on_response(svc::layers().box_http_request())
            .check_service::<Target>()
            // Provides route configuration without pdestination overrides.
            .push(profiles::Layer::without_overrides(
                profiles_client,
                http_profile_route_proxy.into_inner(),
            ))
            .into_new_service()
            // Caches profile stacks.
            .check_new_service_routes::<Profile, Target>()
            .cache(
                svc::layers().push_on_response(
                    svc::layers()
                        // If the service has been unavailable for an extended time, eagerly
                        // fail requests.
                        .push_failfast(dispatch_timeout)
                        // Shares the service, ensuring discovery errors are propagated.
                        .push_spawn_buffer_with_idle_timeout(buffer_capacity, cache_max_idle_age)
                        .push(metrics.stack.layer(stack_labels("profile"))),
                ),
            )
            .instrument(|p: &Profile| info_span!("profile", addr = %p.addr()))
            .check_make_service::<Profile, Target>()
            // Ensures that cache's lock isn't held in poll_ready.
            .push_oneshot()
            .push(router::Layer::new(|()| ProfileTarget))
            .check_new_service_routes::<(), Target>()
            .new_service(());

        svc::stack(http_profile_cache)
            .push_on_response(svc::layers().box_http_response())
            .push_make_ready()
            .push_fallback(
                http_target_cache
                    .push_on_response(svc::layers().box_http_response().box_http_request()),
            )
            .push(admit::AdmitLayer::new(prevent_loop))
            .push_fallback_on_error::<prevent_loop::LoopPrevented, _>(
                svc::stack(http_loopback)
                    .push_on_response(svc::layers().box_http_request())
                    .into_inner(),
            )
            .check_service::<Target>()
            .into_inner()
    }

    pub fn build_server<C, H, S>(
        self,
        listen: transport::Listen<transport::DefaultOrigDstAddr>,
        prevent_loop: impl Into<PreventLoop>,
        tcp_connect: C,
        http_router: H,
        local_identity: tls::Conditional<identity::Local>,
        metrics: ProxyMetrics,
        span_sink: Option<mpsc::Sender<oc::Span>>,
        drain: drain::Watch,
    ) -> serve::Task
    where
        C: tower::Service<TcpEndpoint> + Clone + Send + Sync + 'static,
        C::Error: Into<Error>,
        C::Response: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
        C::Future: Send,
        H: tower::Service<Target, Response = S> + Send + Clone + 'static,
        H::Error: Into<Error>,
        H::Future: Send,
        S: tower::Service<
                http::Request<http::boxed::Payload>,
                Response = http::Response<http::boxed::Payload>,
            > + Send
            + 'static,
        S::Error: Into<Error>,
        S::Future: Send,
    {
        let Config {
            proxy:
                ProxyConfig {
                    server: ServerConfig { h2_settings, .. },
                    disable_protocol_detection_for_ports,
                    dispatch_timeout,
                    max_in_flight_requests,
                    detect_protocol_timeout,
                    ..
                },
            require_identity_for_inbound_ports,
        } = self;

        // The stack is served lazily since some layers (notably buffer) spawn
        // tasks from their constructor. This helps to ensure that tasks are
        // spawned on the same runtime as the proxy.
        // Forwards TCP streams that cannot be decoded as HTTP.
        let tcp_forward = svc::stack(tcp_connect.clone())
            .push(admit::AdmitLayer::new(prevent_loop.into()))
            .push_map_target(|meta: tls::accept::Meta| TcpEndpoint::from(meta.addrs.target_addr()))
            .push(svc::layer::mk(tcp::Forward::new));

        // Strips headers that may be set by the inbound router.
        let http_strip_headers = svc::layers()
            .push(strip_header::request::layer(L5D_REMOTE_IP))
            .push(strip_header::request::layer(L5D_CLIENT_ID))
            .push(strip_header::response::layer(L5D_SERVER_ID));

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

        let http_server_observability = svc::layers()
            .push(TraceContextLayer::new(span_sink.map(|span_sink| {
                SpanConverter::server(span_sink, trace_labels())
            })))
            // Tracks proxy handletime.
            .push(metrics.http_handle_time.layer());

        let http_server = svc::stack(http_router)
            // Ensures that the built service is ready before it is returned
            // to the router to dispatch a request.
            .push_make_ready()
            // Limits the amount of time each request waits to obtain a
            // ready service.
            .push_timeout(dispatch_timeout)
            // Removes the override header after it has been used to
            // determine a reuquest target.
            .push_on_response(strip_header::request::layer(DST_OVERRIDE_HEADER))
            // Routes each request to a target, obtains a service for that
            // target, and dispatches the request.
            .instrument_from_target()
            .push(router::Layer::new(RequestTarget::from))
            .check_new_service::<tls::accept::Meta>()
            // Used by tap.
            .push_http_insert_target()
            .push_on_response(
                svc::layers()
                    .push(http_strip_headers)
                    .push(http_admit_request)
                    .push(http_server_observability)
                    .push(metrics.stack.layer(stack_labels("source")))
                    .box_http_request(),
            )
            .instrument(|src: &tls::accept::Meta| {
                info_span!(
                    "source",
                    target.addr = %src.addrs.target_addr(),
                )
            });

        let tcp_server = Server::new(
            TransportLabels,
            metrics.transport,
            tcp_forward.into_inner(),
            http_server.into_inner(),
            h2_settings,
            drain.clone(),
        );

        let tcp_detect = svc::stack(tcp_server)
            .push(DetectProtocolLayer::new(ProtocolDetect::new(
                disable_protocol_detection_for_ports.clone(),
            )))
            .push(admit::AdmitLayer::new(require_identity_for_inbound_ports))
            // Terminates inbound mTLS from other outbound proxies.
            .push(tls::AcceptTls::layer(
                local_identity,
                disable_protocol_detection_for_ports,
            ))
            // Limits the amount of time that the TCP server spends waiting for TLS handshake &
            // protocol detection. Ensures that connections that never emit data are dropped
            // eventually.
            .push_timeout(detect_protocol_timeout);

        info!(listen.addr = %listen.listen_addr(), "Serving");
        serve::serve(listen, tcp_detect.into_inner(), drain)
    }
}

#[derive(Copy, Clone, Debug)]
struct TransportLabels;

impl transport::metrics::TransportLabels<HttpEndpoint> for TransportLabels {
    type Labels = transport::labels::Key;

    fn transport_labels(&self, _: &HttpEndpoint) -> Self::Labels {
        transport::labels::Key::connect::<()>(
            "inbound",
            tls::Conditional::None(tls::ReasonForNoPeerName::Loopback.into()),
        )
    }
}

impl transport::metrics::TransportLabels<TcpEndpoint> for TransportLabels {
    type Labels = transport::labels::Key;

    fn transport_labels(&self, _: &TcpEndpoint) -> Self::Labels {
        transport::labels::Key::connect::<()>(
            "inbound",
            tls::Conditional::None(tls::ReasonForNoPeerName::Loopback.into()),
        )
    }
}

impl transport::metrics::TransportLabels<ServerProtocol> for TransportLabels {
    type Labels = transport::labels::Key;

    fn transport_labels(&self, proto: &ServerProtocol) -> Self::Labels {
        transport::labels::Key::accept("inbound", proto.tls.peer_identity.as_ref())
    }
}

pub fn trace_labels() -> HashMap<String, String> {
    let mut l = HashMap::new();
    l.insert("direction".to_string(), "inbound".to_string());
    l
}

fn stack_labels(name: &'static str) -> metric_labels::StackLabels {
    metric_labels::StackLabels::inbound(name)
}
