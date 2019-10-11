#![deny(warnings, rust_2018_idioms)]

use linkerd2_app_core::{
    classify,
    config::Config,
    core::resolve::Resolve,
    discover, dns, drain,
    dst::DstAddr,
    errors, http_request_authority_addr, http_request_host_addr,
    http_request_l5d_override_dst_addr, http_request_orig_dst_addr, identity,
    proxy::http::{
        balance, canonicalize, client, fallback, header_from_target, insert,
        metrics as http_metrics, normalize_uri, profiles, retry, router, settings, strip_header,
    },
    proxy::{self, Server},
    reconnect, serve,
    spans::SpanConverter,
    svc, trace_context,
    transport::{self as transport, connect, tls, Source},
    Addr, Conditional, DispatchDeadline, CANONICAL_DST_HEADER, DST_OVERRIDE_HEADER, L5D_CLIENT_ID,
    L5D_REMOTE_IP, L5D_REQUIRE_ID, L5D_SERVER_ID,
};
use opencensus_proto::trace::v1 as oc;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tower_grpc::{self as grpc, generic::client::GrpcService};
use tracing::debug;

#[allow(dead_code)] // TODO #2597
mod add_remote_ip_on_rsp;
#[allow(dead_code)] // TODO #2597
mod add_server_id_on_rsp;
mod endpoint;
mod orig_proto_upgrade;
mod require_identity_on_endpoint;
mod resolve;

pub use self::endpoint::Endpoint;
pub use self::resolve::resolve;

const EWMA_DEFAULT_RTT: Duration = Duration::from_millis(30);
const EWMA_DECAY: Duration = Duration::from_secs(10);

pub fn spawn<R, P>(
    config: &Config,
    local_identity: tls::Conditional<identity::Local>,
    listen: transport::Listen,
    get_original_dst: impl transport::GetOriginalDst + Send + 'static,
    resolve: R,
    dns_resolver: dns::Resolver,
    profiles_client: linkerd2_app_core::profiles::Client<P>,
    tap_layer: linkerd2_app_core::tap::Layer,
    handle_time: http_metrics::handle_time::Scope,
    endpoint_http_metrics: linkerd2_app_core::HttpEndpointMetricsRegistry,
    route_http_metrics: linkerd2_app_core::HttpRouteMetricsRegistry,
    retry_http_metrics: linkerd2_app_core::HttpRouteMetricsRegistry,
    transport_metrics: linkerd2_app_core::transport::MetricsRegistry,
    span_sink: Option<mpsc::Sender<oc::Span>>,
    drain: drain::Watch,
) where
    R: Resolve<DstAddr, Endpoint = Endpoint> + Clone + Send + Sync + 'static,
    R::Future: Send,
    R::Resolution: Send,
    P: GrpcService<grpc::BoxBody> + Clone + Send + Sync + 'static,
    P::ResponseBody: Send,
    <P::ResponseBody as grpc::Body>::Data: Send,
    P::Future: Send,
{
    let capacity = config.outbound_router_capacity;
    let max_idle_age = config.outbound_router_max_idle_age;
    let max_in_flight = config.outbound_max_requests_in_flight;
    let profile_suffixes = config.destination_profile_suffixes.clone();
    let canonicalize_timeout = config.dns_canonicalize_timeout;
    let dispatch_timeout = config.outbound_dispatch_timeout;

    let mut trace_labels = HashMap::new();
    trace_labels.insert("direction".to_string(), "outbound".to_string());

    // Establishes connections to remote peers (for both TCP
    // forwarding and HTTP proxying).
    let connect = svc::stack(connect::svc(config.outbound_connect_keepalive))
        .push(tls::client::layer(local_identity))
        .push_timeout(config.outbound_connect_timeout)
        .push(transport_metrics.layer_connect(TransportLabels));

    let trace_context_layer = trace_context::layer(
        span_sink
            .clone()
            .map(|span_sink| SpanConverter::client(span_sink, trace_labels.clone())),
    );
    // Instantiates an HTTP client for for a `client::Config`
    let client_stack = connect
        .clone()
        .push(client::layer("out", config.h2_settings))
        .push(reconnect::layer({
            let backoff = config.outbound_connect_backoff.clone();
            move |_| Ok(backoff.stream())
        }))
        .push(trace_context_layer)
        .push(normalize_uri::layer());

    // A per-`outbound::Endpoint` stack that:
    //
    // 1. Records http metrics  with per-endpoint labels.
    // 2. Instruments `tap` inspection.
    // 3. Changes request/response versions when the endpoint
    //    supports protocol upgrade (and the request may be upgraded).
    // 4. Appends `l5d-server-id` to responses coming back iff meshed
    //    TLS was used on the connection.
    // 5. Routes requests to the correct client (based on the
    //    request version and headers).
    // 6. Strips any `l5d-server-id` that may have been received from
    //    the server, before we apply our own.
    let endpoint_stack = client_stack
        .push(strip_header::response::layer(L5D_REMOTE_IP))
        .push(strip_header::response::layer(L5D_SERVER_ID))
        .push(strip_header::request::layer(L5D_REQUIRE_ID))
        // disabled due to information leagkage
        //.push(add_remote_ip_on_rsp::layer())
        //.push(add_server_id_on_rsp::layer())
        .push(orig_proto_upgrade::layer())
        .push(tap_layer.clone())
        .push(http_metrics::layer::<_, classify::Response>(
            endpoint_http_metrics,
        ))
        .push(require_identity_on_endpoint::layer())
        .serves::<Endpoint>();

    // A per-`dst::Route` layer that uses profile data to configure
    // a per-route layer.
    //
    // 1. The `classify` module installs a `classify::Response`
    //    extension into each request so that all lower metrics
    //    implementations can use the route-specific configuration.
    // 2. A timeout is optionally enabled if the target `dst::Route`
    //    specifies a timeout. This goes before `retry` to cap
    //    retries.
    // 3. Retries are optionally enabled depending on if the route
    //    is retryable.
    let dst_route_layer = svc::layers()
        .push(insert::target::layer())
        .push(http_metrics::layer::<_, classify::Response>(
            retry_http_metrics.clone(),
        ))
        .push(retry::layer(retry_http_metrics.clone()))
        .push(proxy::http::timeout::layer())
        .push(http_metrics::layer::<_, classify::Response>(
            route_http_metrics,
        ))
        .push(classify::layer())
        .push_buffer_pending(max_in_flight, DispatchDeadline::extract);

    // Routes requests to their original destination endpoints. Used as
    // a fallback when service discovery has no endpoints for a destination.
    //
    // If the `l5d-require-id` header is present, then that identity is
    // used as the server name when connecting to the endpoint.
    let orig_dst_router_layer = svc::layers()
        .push_buffer_pending(max_in_flight, DispatchDeadline::extract)
        .push(router::layer(
            router::Config::new("out ep", capacity, max_idle_age),
            |req: &http::Request<_>| {
                let ep = Endpoint::from_request(req);
                debug!("outbound ep={:?}", ep);
                ep
            },
        ));

    // Resolves the target via the control plane and balances requests
    // over all endpoints returned from the destination service.
    const DISCOVER_UPDATE_BUFFER_CAPACITY: usize = 2;
    let balancer_layer = svc::layers()
        .push_spawn_ready()
        .push(discover::Layer::new(
            DISCOVER_UPDATE_BUFFER_CAPACITY,
            resolve,
        ))
        .push(balance::layer(EWMA_DEFAULT_RTT, EWMA_DECAY));

    // If the balancer fails to be created, i.e., because it is unresolvable,
    // fall back to using a router that dispatches request to the
    // application-selected original destination.
    let distributor = endpoint_stack
        .push(fallback::layer(balancer_layer, orig_dst_router_layer))
        .serves::<DstAddr>();

    // A per-`DstAddr` stack that does the following:
    //
    // 1. Adds the `CANONICAL_DST_HEADER` from the `DstAddr`.
    // 2. Determines the profile of the destination and applies
    //    per-route policy.
    // 3. Creates a load balancer , configured by resolving the
    //   `DstAddr` with a resolver.
    let dst_stack = distributor
        .push_buffer_pending(max_in_flight, DispatchDeadline::extract)
        .push(profiles::router::layer(
            profile_suffixes,
            profiles_client,
            dst_route_layer,
        ))
        .push(header_from_target::layer(CANONICAL_DST_HEADER));

    // Routes request using the `DstAddr` extension.
    //
    // This is shared across addr-stacks so that multiple addrs that
    // canonicalize to the same DstAddr use the same dst-stack service.
    let dst_router = dst_stack
        .push_buffer_pending(max_in_flight, DispatchDeadline::extract)
        .push(router::layer(
            router::Config::new("out dst", capacity, max_idle_age),
            |req: &http::Request<_>| {
                let addr = req.extensions().get::<Addr>().cloned().map(|addr| {
                    let settings = settings::Settings::from_request(req);
                    DstAddr::outbound(addr, settings)
                });
                debug!("outbound dst={:?}", addr);
                addr
            },
        ))
        .into_inner()
        .make();

    // Canonicalizes the request-specified `Addr` via DNS, and
    // annotates each request with a refined `Addr` so that it may be
    // routed by the dst_router.
    let addr_stack = svc::stack(svc::Shared::new(dst_router))
        .push(canonicalize::layer(dns_resolver, canonicalize_timeout));

    // Routes requests to an `Addr`:
    //
    // 1. If the request had an `l5d-override-dst` header, this value
    // is used.
    //
    // 2. If the request is HTTP/2 and has an :authority, this value
    // is used.
    //
    // 3. If the request is absolute-form HTTP/1, the URI's
    // authority is used.
    //
    // 4. If the request has an HTTP/1 Host header, it is used.
    //
    // 5. Finally, if the Source had an SO_ORIGINAL_DST, this TCP
    // address is used.
    let addr_router = addr_stack
        .push(strip_header::request::layer(L5D_CLIENT_ID))
        .push(strip_header::request::layer(DST_OVERRIDE_HEADER))
        .push(insert::target::layer())
        .push_buffer_pending(max_in_flight, DispatchDeadline::extract)
        .push(router::layer(
            router::Config::new("out addr", capacity, max_idle_age),
            |req: &http::Request<_>| {
                http_request_l5d_override_dst_addr(req)
                    .map(|override_addr| {
                        debug!("outbound addr={:?}; dst-override", override_addr);
                        override_addr
                    })
                    .or_else(|_| {
                        let addr = http_request_authority_addr(req)
                            .or_else(|_| http_request_host_addr(req))
                            .or_else(|_| http_request_orig_dst_addr(req));
                        debug!("outbound addr={:?}", addr);
                        addr
                    })
                    .ok()
            },
        ))
        .into_inner()
        .make();

    // Share a single semaphore across all requests to signal when
    // the proxy is overloaded.
    let admission_control = svc::stack(addr_router)
        .push_concurrency_limit(max_in_flight)
        .push_load_shed();

    let trace_context_layer = trace_context::layer(
        span_sink.map(|span_sink| SpanConverter::server(span_sink, trace_labels)),
    );
    // Instantiates an HTTP service for each `Source` using the
    // shared `addr_router`. The `Source` is stored in the request's
    // extensions so that it can be used by the `addr_router`.
    let server_stack = svc::stack(svc::Shared::new(admission_control))
        .push(insert::layer(move || {
            DispatchDeadline::after(dispatch_timeout)
        }))
        .push(insert::target::layer())
        .push(errors::layer())
        .push(trace_context_layer)
        .push(handle_time.layer());

    let proxy = Server::new(
        "out",
        listen.local_addr(),
        TransportLabels,
        transport_metrics,
        connect,
        server_stack,
        config.h2_settings,
        drain.clone(),
    );

    let no_tls: tls::Conditional<identity::Local> =
        Conditional::None(tls::ReasonForNoPeerName::Loopback.into());
    let skip_ports = config
        .outbound_ports_disable_protocol_detection
        .iter()
        .map(|p| *p);
    let accept = tls::AcceptTls::new(get_original_dst, no_tls, proxy)
        .without_protocol_detection_for(skip_ports);

    serve::spawn("out", listen, accept, drain);
}

#[derive(Copy, Clone, Debug)]
struct TransportLabels;

impl transport::metrics::TransportLabels<Endpoint> for TransportLabels {
    type Labels = transport::labels::Key;

    fn transport_labels(&self, endpoint: &Endpoint) -> Self::Labels {
        transport::labels::Key::connect("outbound", endpoint.identity.as_ref())
    }
}

impl transport::metrics::TransportLabels<Source> for TransportLabels {
    type Labels = transport::labels::Key;

    fn transport_labels(&self, source: &Source) -> Self::Labels {
        transport::labels::Key::accept("outbound", source.tls_peer.as_ref())
    }
}
