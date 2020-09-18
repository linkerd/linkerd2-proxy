//! Configures and runs the inbound proxy.
//!
//! The inbound proxy is responsible for terminating traffic from other network
//! endpoints inbound to the local application.

#![deny(warnings, rust_2018_idioms)]

pub use self::endpoint::{HttpEndpoint, ProfileTarget, RequestTarget, Target, TcpEndpoint};
use self::prevent_loop::PreventLoop;
use self::require_identity_for_ports::RequireIdentityForPorts;
use futures::future;
use linkerd2_app_core::{
    admit, classify,
    config::{ProxyConfig, ServerConfig},
    drain, dst, errors, metric_labels,
    opencensus::proto::trace::v1 as oc,
    profiles,
    proxy::{
        http::{self, orig_proto, strip_header, DetectHttp},
        identity, tap, tcp,
    },
    reconnect, router,
    spans::SpanConverter,
    svc::{self, NewService},
    transport::{self, io, listen, tls},
    Error, ProxyMetrics, TraceContextLayer, DST_OVERRIDE_HEADER,
};
use std::collections::HashMap;
use tokio::{net::TcpStream, sync::mpsc};
use tracing::debug_span;

pub mod endpoint;
mod prevent_loop;
mod require_identity_for_ports;

type SensorIo<T> = io::SensorIo<T, transport::metrics::Sensor>;

#[derive(Clone, Debug)]
pub struct Config {
    pub proxy: ProxyConfig,
    pub require_identity_for_inbound_ports: RequireIdentityForPorts,
}

impl Config {
    pub fn build<L, S, P>(
        self,
        listen_addr: std::net::SocketAddr,
        local_identity: tls::Conditional<identity::Local>,
        http_loopback: L,
        profiles_client: P,
        tap_layer: tap::Layer,
        metrics: ProxyMetrics,
        span_sink: Option<mpsc::Sender<oc::Span>>,
        drain: drain::Watch,
    ) -> impl tower::Service<
        listen::Addrs,
        Error = impl Into<Error>,
        Future = impl Send + 'static,
        Response = impl tower::Service<
            tokio::net::TcpStream,
            Response = (),
            Error = impl Into<Error>,
            Future = impl Send + 'static,
        > + Send
                       + 'static,
    > + Send
           + 'static
    where
        L: tower::Service<Target, Response = S> + Unpin + Clone + Send + Sync + 'static,
        L::Error: Into<Error>,
        L::Future: Unpin + Send,
        S: tower::Service<
                http::Request<http::boxed::Payload>,
                Response = http::Response<http::boxed::Payload>,
            > + Unpin
            + Send
            + 'static,
        S::Error: Into<Error>,
        S::Future: Unpin + Send,
        P: profiles::GetProfile<Target> + Unpin + Clone + Send + 'static,
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
        metrics: &ProxyMetrics,
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
            .push(metrics.transport.layer_connect(TransportLabels))
            .push(admit::AdmitLayer::new(prevent_loop))
            .into_inner()
    }

    pub fn build_http_router<C, P, L, S>(
        &self,
        tcp_connect: C,
        prevent_loop: impl Into<PreventLoop>,
        loopback: L,
        profiles_client: P,
        tap_layer: tap::Layer,
        metrics: ProxyMetrics,
        span_sink: Option<mpsc::Sender<oc::Span>>,
    ) -> impl tower::Service<
        Target,
        Error = Error,
        Future = impl Send,
        Response = impl tower::Service<
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
        P: profiles::GetProfile<Target> + Unpin + Clone + Send + 'static,
        P::Future: Unpin + Send,
        P::Error: Send,
        // The loopback router processes requests sent to the inbound port.
        L: tower::Service<Target, Response = S> + Unpin + Send + Clone + 'static,
        L::Error: Into<Error>,
        L::Future: Unpin + Send,
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
            .push(http::client::layer(connect.h2_settings))
            .push(reconnect::layer({
                let backoff = connect.backoff.clone();
                move |_| Ok(backoff.stream())
            }))
            .check_make_service::<HttpEndpoint, http::Request<_>>();

        let observe = svc::layers()
            // Registers the stack to be tapped.
            .push(tap_layer)
            // Records metrics for each `Target`.
            .push(metrics.http_endpoint.into_layer::<classify::Response>())
            .push_on_response(TraceContextLayer::new(
                span_sink
                    .clone()
                    .map(|span_sink| SpanConverter::client(span_sink, trace_labels())),
            ));

        let target = endpoint
            .push_map_target(HttpEndpoint::from)
            .push(observe)
            .into_new_service()
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
            .push(profiles::discover::layer(profiles_client))
            .into_new_service()
            .cache(
                svc::layers().push_on_response(
                    svc::layers()
                        .push_failfast(dispatch_timeout)
                        .push_spawn_buffer_with_idle_timeout(buffer_capacity, cache_max_idle_age)
                        .push(metrics.stack.layer(stack_labels("profile")))
                        .box_http_response(),
                ),
            )
            .spawn_buffer(buffer_capacity)
            .instrument(|_: &Target| debug_span!("profile"))
            .check_make_service::<Target, http::Request<_>>();

        let forward = target
            .cache(
                svc::layers().push_on_response(
                    svc::layers()
                        .push_failfast(dispatch_timeout)
                        .push_spawn_buffer_with_idle_timeout(buffer_capacity, cache_max_idle_age)
                        .push(metrics.stack.layer(stack_labels("forward")))
                        .box_http_response(),
                ),
            )
            .spawn_buffer(buffer_capacity)
            .instrument(|_: &Target| debug_span!("forward"))
            .check_make_service::<Target, http::Request<http::boxed::Payload>>();

        // Attempts to resolve the target as a service profile or, if that
        // fails, skips that stack to forward to the local endpoint.
        profile
            .push_make_ready()
            .push_fallback(forward)
            // If the traffic is targeted at the inbound port, send it through
            // the loopback service (i.e. as a gateway). This is done before
            // caching so that the loopback stack can determine whether it
            // should cache or not.
            .push(admit::AdmitLayer::new(prevent_loop))
            .push_fallback_on_error::<prevent_loop::LoopPrevented, _>(
                svc::stack(loopback)
                    .check_make_service::<Target, http::Request<_>>()
                    .into_inner(),
            )
            .check_make_service::<Target, http::Request<http::boxed::Payload>>()
            .into_inner()
    }

    pub fn build_accept<I, F, A, H, S>(
        &self,
        tcp_forward: F,
        http_router: H,
        metrics: ProxyMetrics,
        span_sink: Option<mpsc::Sender<oc::Span>>,
        drain: drain::Watch,
    ) -> impl tower::Service<
        tls::accept::Meta,
        Error = impl Into<Error>,
        Future = impl Send + 'static,
        Response = impl tower::Service<
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
        I: io::AsyncRead + io::AsyncWrite + Unpin + Send + 'static,
        F: tower::Service<TcpEndpoint, Response = A> + Unpin + Clone + Send + 'static,
        F::Error: Into<Error>,
        F::Future: Send,
        A: tower::Service<io::PrefixedIo<I>, Response = ()> + Clone + Send + 'static,
        A::Error: Into<Error>,
        A::Future: Send,
        H: tower::Service<Target, Response = S, Error = Error> + Unpin + Clone + Send + 'static,
        H::Future: Send,
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

        let http_server_observability = svc::layers().push(TraceContextLayer::new(
            span_sink.map(|span_sink| SpanConverter::server(span_sink, trace_labels())),
        ));

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
            // Used by tap.
            .push_http_insert_target()
            .check_new_service::<tls::accept::Meta, http::Request<_>>()
            .push(svc::layer::mk(http::normalize_uri::MakeNormalizeUri::new))
            .push_on_response(
                svc::layers()
                    .push(http_admit_request)
                    .push(http_server_observability)
                    .push(metrics.stack.layer(stack_labels("source")))
                    .box_http_request()
                    .box_http_response(),
            )
            .instrument(|_: &_| debug_span!("source"))
            .check_new_service::<tls::accept::Meta, http::Request<_>>()
            .into_inner()
            .into_make_service();

        DetectHttp::new(
            h2_settings,
            detect_protocol_timeout,
            http_server,
            svc::stack(tcp_forward)
                .push_map_target(TcpEndpoint::from)
                .into_inner(),
            drain.clone(),
        )
    }

    pub fn build_tls_accept<D, A, F, B>(
        self,
        detect: D,
        tcp_forward: F,
        identity: tls::Conditional<identity::Local>,
        metrics: ProxyMetrics,
    ) -> impl tower::Service<
        listen::Addrs,
        Error = impl Into<Error>,
        Future = impl Send + 'static,
        Response = impl tower::Service<
            TcpStream,
            Response = (),
            Error = impl Into<Error>,
            Future = impl Send + 'static,
        > + Send
                       + 'static,
    > + Send
           + 'static
    where
        D: tower::Service<tls::accept::Meta, Response = A> + Unpin + Clone + Send + Sync + 'static,
        D::Error: Into<Error>,
        D::Future: Unpin + Send,
        A: tower::Service<SensorIo<io::BoxedIo>, Response = ()> + Unpin + Send + 'static,
        A::Error: Into<Error>,
        A::Future: Send,
        F: tower::Service<TcpEndpoint, Response = B> + Unpin + Clone + Send + Sync + 'static,
        F::Error: Into<Error>,
        F::Future: Unpin + Send,
        B: tower::Service<SensorIo<TcpStream>, Response = ()> + Unpin + Send + 'static,
        B::Error: Into<Error>,
        B::Future: Send,
    {
        let ProxyConfig {
            disable_protocol_detection_for_ports: skip_detect,
            detect_protocol_timeout,
            ..
        } = self.proxy;
        let require_identity = self.require_identity_for_inbound_ports;

        svc::stack::MakeSwitch::new(
            skip_detect,
            svc::stack(detect)
                .push(admit::AdmitLayer::new(require_identity))
                .push(metrics.transport.layer_accept(TransportLabels))
                .push(tls::DetectTls::layer(
                    identity.clone(),
                    detect_protocol_timeout,
                ))
                .into_inner(),
            svc::stack(tcp_forward)
                .push_map_target(TcpEndpoint::from)
                .push(metrics.transport.layer_accept(TransportLabels))
                .into_inner(),
        )
    }
}

#[derive(Copy, Clone, Debug)]
struct TransportLabels;

impl transport::metrics::TransportLabels<HttpEndpoint> for TransportLabels {
    type Labels = transport::labels::Key;

    fn transport_labels(&self, _: &HttpEndpoint) -> Self::Labels {
        transport::labels::Key::Connect(transport::labels::EndpointLabels {
            direction: transport::labels::Direction::In,
            authority: None,
            labels: None,
            tls_id: tls::Conditional::None(tls::ReasonForNoPeerName::Loopback.into()).into(),
        })
    }
}

impl transport::metrics::TransportLabels<TcpEndpoint> for TransportLabels {
    type Labels = transport::labels::Key;

    fn transport_labels(&self, _: &TcpEndpoint) -> Self::Labels {
        transport::labels::Key::Connect(transport::labels::EndpointLabels {
            direction: transport::labels::Direction::In,
            authority: None,
            labels: None,
            tls_id: tls::Conditional::None(tls::ReasonForNoPeerName::Loopback.into()).into(),
        })
    }
}

impl transport::metrics::TransportLabels<listen::Addrs> for TransportLabels {
    type Labels = transport::labels::Key;

    fn transport_labels(&self, _: &listen::Addrs) -> Self::Labels {
        transport::labels::Key::Accept(
            transport::labels::Direction::In,
            tls::Conditional::<()>::None(tls::ReasonForNoPeerName::PortSkipped).into(),
        )
    }
}

impl transport::metrics::TransportLabels<tls::accept::Meta> for TransportLabels {
    type Labels = transport::labels::Key;

    fn transport_labels(&self, target: &tls::accept::Meta) -> Self::Labels {
        transport::labels::Key::Accept(
            transport::labels::Direction::In,
            target.peer_identity.as_ref().into(),
        )
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
