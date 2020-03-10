//! Configures and runs the inbound proxy.
//!
//! The inbound proxy is responsible for terminating traffic from other network
//! endpoints inbound to the local application.

#![deny(warnings, rust_2018_idioms)]

pub use self::endpoint::{
    HttpEndpoint, Profile, ProfileTarget, RequestTarget, Target, TcpEndpoint,
};
use futures::future;
use linkerd2_app_core::{
    classify,
    config::{ProxyConfig, ServerConfig},
    drain, dst, errors, metric_labels,
    opencensus::proto::trace::v1 as oc,
    profiles,
    proxy::{
        self,
        http::{self, normalize_uri, orig_proto, strip_header},
        identity,
        server::{Protocol as ServerProtocol, Server},
        tap, tcp,
    },
    reconnect, router, serve,
    spans::SpanConverter,
    svc::{self, NewService},
    transport::{self, io::BoxedIo, tls, OrigDstAddr, SysOrigDstAddr},
    Error, ProxyMetrics, TraceContextLayer, DST_OVERRIDE_HEADER, L5D_CLIENT_ID, L5D_REMOTE_IP,
    L5D_SERVER_ID,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use tokio::sync::mpsc;
use tracing::{info, info_span};

mod endpoint;
#[allow(dead_code)] // TODO #2597
mod set_client_id_on_req;
#[allow(dead_code)] // TODO #2597
mod set_remote_ip_on_req;

#[derive(Clone, Debug)]
pub struct Config<A: OrigDstAddr = SysOrigDstAddr> {
    pub proxy: ProxyConfig<A>,
}

pub struct Inbound {
    pub listen_addr: SocketAddr,
    pub serve: serve::Task,
}

impl<A: OrigDstAddr> Config<A> {
    pub fn with_orig_dst_addr<B: OrigDstAddr>(self, orig_dst_addr: B) -> Config<B> {
        Config {
            proxy: self.proxy.with_orig_dst_addr(orig_dst_addr),
        }
    }

    pub fn build<P>(
        self,
        local_identity: tls::Conditional<identity::Local>,
        profiles_client: P,
        tap_layer: tap::Layer,
        metrics: ProxyMetrics,
        span_sink: Option<mpsc::Sender<oc::Span>>,
        drain: drain::Watch,
    ) -> Result<Inbound, Error>
    where
        A: Send + 'static,
        P: profiles::GetRoutes<Profile> + Clone + Send + 'static,
        P::Future: Send,
    {
        use proxy::core::listen::{Bind, Listen};
        let Config {
            proxy:
                ProxyConfig {
                    server: ServerConfig { bind, h2_settings },
                    connect,
                    buffer_capacity,
                    cache_max_idle_age,
                    disable_protocol_detection_for_ports,
                    dispatch_timeout,
                    max_in_flight_requests,
                },
        } = self;

        let listen = bind.bind().map_err(Error::from)?;
        let listen_addr = listen.listen_addr();

        // The stack is served lazily since some layers (notably buffer) spawn
        // tasks from their constructor. This helps to ensure that tasks are
        // spawned on the same runtime as the proxy.
        let serve = Box::new(future::lazy(move || {
            // Establishes connections to the local application (for both
            // TCP forwarding and HTTP proxying).
            let tcp_connect = svc::connect(connect.keepalive)
                .push_map_response(BoxedIo::new) // Ensures the transport propagates shutdown properly.
                .push_timeout(connect.timeout)
                .push(metrics.transport.layer_connect(TransportLabels));

            // Forwards TCP streams that cannot be decoded as HTTP.
            let tcp_forward = tcp_connect
                .clone()
                .push_map_target(|meta: tls::accept::Meta| {
                    TcpEndpoint::from(meta.addrs.target_addr())
                })
                .push(svc::layer::mk(tcp::Forward::new));

            // Caches HTTP clients for each inbound port & HTTP settings.
            let http_endpoint_cache = tcp_connect
                .push(http::MakeClientLayer::new(connect.h2_settings))
                .push(reconnect::layer({
                    let backoff = connect.backoff.clone();
                    move |_| Ok(backoff.stream())
                }))
                .into_new_service()
                .cache(
                    svc::layers().push_on_response(
                        svc::layers()
                            // If the service has been ready & unused for `cache_max_idle_age`,
                            // fail it.
                            .push_idle_timeout(cache_max_idle_age)
                            // If the service has been unavailable for an extend time, eagerly
                            // fail requests.
                            .push_failfast(dispatch_timeout)
                            // Shares the service, ensuring discovery errors are propagated.
                            .push_spawn_buffer(buffer_capacity)
                            .push(metrics.stack.layer(stack_labels("endpoint"))),
                    ),
                )
                .instrument(|ep: &HttpEndpoint| {
                    info_span!(
                        "endpoint",
                        port = %ep.port,
                        http = ?ep.settings,
                    )
                })
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

            let http_target_cache = http_endpoint_cache
                .push_map_target(HttpEndpoint::from)
                // Normalizes the URI, i.e. if it was originally in
                // absolute-form on the outbound side.
                .push(normalize_uri::layer())
                .push(http_target_observability)
                .into_new_service()
                .cache(
                    svc::layers().push_on_response(
                        svc::layers()
                            // If the service has been ready & unused for `cache_max_idle_age`,
                            // fail it.
                            .push_idle_timeout(cache_max_idle_age)
                            // If the service has been unavailable for an extend time, eagerly
                            // fail requests.
                            .push_failfast(dispatch_timeout)
                            // Shares the service, ensuring discovery errors are propagated.
                            .push_spawn_buffer(buffer_capacity)
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
                            // If the service has been ready & unused for `cache_max_idle_age`,
                            // fail it.
                            .push_idle_timeout(cache_max_idle_age)
                            // If the service has been unavailable for an extend time, eagerly
                            // fail requests.
                            .push_failfast(dispatch_timeout)
                            // Shares the service, ensuring discovery errors are propagated.
                            .push_spawn_buffer(buffer_capacity)
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
                // Eagerly fail requests when the proxy is out of capacity for some time period.
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

            let http_server = svc::stack(http_profile_cache)
                .push_on_response(svc::layers().box_http_response())
                .push_make_ready()
                .push_fallback(
                    http_target_cache
                        .push_on_response(svc::layers().box_http_response().box_http_request()),
                )
                .check_service::<Target>()
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
                .push_on_response(http_strip_headers)
                .push_on_response(http_admit_request)
                .push_on_response(http_server_observability)
                .push_on_response(metrics.stack.layer(stack_labels("source")))
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
                disable_protocol_detection_for_ports.clone(),
            );

            // Terminate inbound mTLS from other outbound proxies.
            let accept = tls::AcceptTls::new(local_identity, tcp_server)
                .with_skip_ports(disable_protocol_detection_for_ports);

            info!(listen.addr = %listen.listen_addr(), "Serving");
            serve::serve(listen, accept, drain)
        }));

        Ok(Inbound { listen_addr, serve })
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
