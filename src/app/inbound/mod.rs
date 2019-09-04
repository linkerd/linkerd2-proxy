use super::{classify, config::Config, dst::DstAddr, identity, DispatchDeadline};
use crate::proxy::http::{
    client, insert, metrics as http_metrics, normalize_uri, profiles, router, settings,
    strip_header,
};
use crate::proxy::{accept, reconnect, Server};
use crate::transport::{self, connect, keepalive, tls, Connection};
use crate::{core::listen::ServeConnection, svc, trace_context, Addr};
use std::net::SocketAddr;
use tokio::sync::mpsc;
use tower_grpc::{self as grpc, generic::client::GrpcService};
use tracing::debug;

mod endpoint;
mod orig_proto_downgrade;
mod rewrite_loopback_addr;
#[allow(dead_code)] // TODO #2597
mod set_client_id_on_req;
#[allow(dead_code)] // TODO #2597
mod set_remote_ip_on_req;

pub use self::endpoint::{Endpoint, RecognizeEndpoint};

pub fn server<P>(
    config: &Config,
    local_identity: tls::Conditional<identity::Local>,
    local_addr: SocketAddr,
    profiles_client: super::profiles::Client<P>,
    tap_layer: crate::tap::Layer,
    handle_time: http_metrics::handle_time::Scope,
    endpoint_http_metrics: super::HttpEndpointMetricsRegistry,
    route_http_metrics: super::HttpRouteMetricsRegistry,
    transport_metrics: transport::metrics::Registry,
    span_sink: mpsc::Sender<trace_context::Span>,
) -> impl ServeConnection<Connection>
where
    P: GrpcService<grpc::BoxBody> + Clone + Send + Sync + 'static,
    P::ResponseBody: Send,
    <P::ResponseBody as grpc::Body>::Data: Send,
    P::Future: Send,
{
    let capacity = config.inbound_router_capacity;
    let max_idle_age = config.inbound_router_max_idle_age;
    let max_in_flight = config.inbound_max_requests_in_flight;
    let profile_suffixes = config.destination_profile_suffixes.clone();
    let default_fwd_addr = config.inbound_forward.map(|a| a.into());
    let dispatch_timeout = config.inbound_dispatch_timeout;

    // Establishes connections to the local application (for both
    // TCP forwarding and HTTP proxying).
    let connect = svc::builder()
        .layer(rewrite_loopback_addr::layer())
        .layer(transport_metrics.connect("inbound"))
        .timeout(config.inbound_connect_timeout)
        .layer(keepalive::connect::layer(config.inbound_connect_keepalive))
        .layer(tls::client::layer(local_identity))
        .service(connect::svc());

    // Instantiates an HTTP client for a `client::Config`
    let client_stack = svc::builder()
        .layer(normalize_uri::layer())
        .layer(trace_context::layer(span_sink))
        .layer(reconnect::layer().with_backoff(config.inbound_connect_backoff.clone()))
        .layer(client::layer("in", config.h2_settings))
        .service(connect.clone());

    // A stack configured by `router::Config`, responsible for building
    // a router made of route stacks configured by `inbound::Endpoint`.
    //
    // If there is no `SO_ORIGINAL_DST` for an inbound socket,
    // `default_fwd_addr` may be used.
    let endpoint_router = svc::builder()
        .layer(router::layer(
            router::Config::new("in endpoint", capacity, max_idle_age),
            RecognizeEndpoint::new(default_fwd_addr),
        ))
        .buffer_pending(max_in_flight, DispatchDeadline::extract)
        .layer(http_metrics::layer::<_, classify::Response>(
            endpoint_http_metrics,
        ))
        .layer(tap_layer)
        .service(client_stack)
        .make();

    // A per-`dst::Route` layer that uses profile data to configure
    // a per-route layer.
    //
    // The `classify` module installs a `classify::Response`
    // extension into each request so that all lower metrics
    // implementations can use the route-specific configuration.
    let dst_route_stack = svc::builder()
        .buffer_pending(max_in_flight, DispatchDeadline::extract)
        .layer(classify::layer())
        .layer(http_metrics::layer::<_, classify::Response>(
            route_http_metrics,
        ))
        .layer(insert::target::layer());

    // A per-`DstAddr` stack that does the following:
    //
    // 1. Determines the profile of the destination and applies
    //    per-route policy.
    // 2. Annotates the request with the `DstAddr` so that
    //    `RecognizeEndpoint` can use the value.
    let dst_stack = svc::builder()
        .layer(strip_header::request::layer(super::DST_OVERRIDE_HEADER))
        .layer(profiles::router::layer(
            profile_suffixes,
            profiles_client,
            dst_route_stack,
        ))
        .buffer_pending(max_in_flight, DispatchDeadline::extract)
        .layer(insert::target::layer())
        .service(svc::shared(endpoint_router));

    // Routes requests to a `DstAddr`.
    //
    // 1. If the CANONICAL_DST_HEADER is set by the remote peer,
    // this value is used to construct a DstAddr.
    //
    // 2. If the OVERRIDE_DST_HEADER is set by the remote peer,
    // this value is used.
    //
    // 3. If the request is HTTP/2 and has an :authority, this value
    // is used.
    //
    // 4. If the request is absolute-form HTTP/1, the URI's
    // authority is used.
    //
    // 5. If the request has an HTTP/1 Host header, it is used.
    //
    // 6. Finally, if the Source had an SO_ORIGINAL_DST, this TCP
    // address is used.
    let dst_router = svc::builder()
        .layer(router::layer(
            router::Config::new("in dst", capacity, max_idle_age),
            |req: &http::Request<_>| {
                let canonical = req
                    .headers()
                    .get(super::CANONICAL_DST_HEADER)
                    .and_then(|dst| dst.to_str().ok())
                    .and_then(|d| Addr::from_str(d).ok());
                debug!("inbound canonical={:?}", canonical);

                let dst = canonical
                    .or_else(|| {
                        super::http_request_l5d_override_dst_addr(req)
                            .map(|override_addr| {
                                debug!("inbound dst={:?}; dst-override", override_addr);
                                override_addr
                            })
                            .ok()
                    })
                    .or_else(|| super::http_request_authority_addr(req).ok())
                    .or_else(|| super::http_request_host_addr(req).ok())
                    .or_else(|| super::http_request_orig_dst_addr(req).ok());
                debug!("inbound dst={:?}", dst);

                dst.map(|addr| {
                    let settings = settings::Settings::from_request(req);
                    DstAddr::inbound(addr, settings)
                })
            },
        ))
        .buffer_pending(max_in_flight, DispatchDeadline::extract)
        .service(dst_stack)
        .make();

    // Share a single semaphore across all requests to signal when
    // the proxy is overloaded.
    let admission_control = svc::builder()
        .load_shed()
        .concurrency_limit(max_in_flight)
        .service(dst_router);

    // As HTTP requests are accepted, the `Source` connection
    // metadata is stored on each request's extensions.
    //
    // Furthermore, HTTP/2 requests may be downgraded to HTTP/1.1 per
    // `orig-proto` headers. This happens in the source stack so that
    // the router need not detect whether a request _will be_ downgraded.
    let source_stack = svc::builder()
        .layer(handle_time.layer())
        .layer(super::errors::layer())
        .layer(insert::layer(move || {
            DispatchDeadline::after(dispatch_timeout)
        }))
        .layer(strip_header::response::layer(super::L5D_SERVER_ID))
        .layer(strip_header::request::layer(super::L5D_CLIENT_ID))
        .layer(strip_header::request::layer(super::L5D_REMOTE_IP))
        .layer(insert::target::layer())
        .layer(orig_proto_downgrade::layer())
        // disabled on purpose
        //.push(set_remote_ip_on_req::layer())
        //.push(set_client_id_on_req::layer())
        .service(svc::shared(admission_control));

    // As the inbound proxy accepts connections, we don't do any
    // special transport-level handling.
    let accept = accept::builder()
        .layer(transport_metrics.accept("inbound"))
        .layer(keepalive::accept::layer(config.inbound_accept_keepalive));

    Server::new(
        "out",
        local_addr,
        accept,
        connect,
        source_stack,
        config.h2_settings,
    )
}
