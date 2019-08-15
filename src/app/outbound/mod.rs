use super::{classify, config::Config, dst::DstAddr, identity, DispatchDeadline};
use crate::control::destination::{Metadata, Unresolvable};
use crate::proxy::http::{
    balance, canonicalize, client, fallback, header_from_target, insert, metrics as http_metrics,
    normalize_uri, profiles, retry, router, settings, strip_header,
};
use crate::core::listen::ServeConnection;
use crate::core::resolve::{Resolve, Resolution};
use crate::proxy::{self, accept, resolve, reconnect, Server};
use crate::transport::Connection;
use crate::transport::{self, connect, keepalive, tls};
use crate::{svc, Addr, NameAddr};
use std::net::SocketAddr;
use std::time::Duration;
use tower_grpc::{self as grpc, generic::client::GrpcService};
use tracing::debug;

#[allow(dead_code)] // TODO #2597
mod add_remote_ip_on_rsp;
#[allow(dead_code)] // TODO #2597
mod add_server_id_on_rsp;
mod discovery;
mod endpoint;
mod orig_proto_upgrade;
mod require_identity_on_endpoint;

pub(super) use self::endpoint::Endpoint;
pub(super) use self::require_identity_on_endpoint::RequireIdentityError;

const EWMA_DEFAULT_RTT: Duration = Duration::from_millis(30);
const EWMA_DECAY: Duration = Duration::from_secs(10);

pub fn server<R, P>(
    config: &Config,
    local_identity: tls::Conditional<identity::Local>,
    local_addr: SocketAddr,
    resolve: R,
    dns_resolver: crate::dns::Resolver,
    profiles_client: super::profiles::Client<P>,
    tap_layer: crate::tap::Layer,
    handle_time: http_metrics::handle_time::Scope,
    endpoint_http_metrics: super::HttpEndpointMetricsRegistry,
    route_http_metrics: super::HttpRouteMetricsRegistry,
    retry_http_metrics: super::HttpRouteMetricsRegistry,
    transport_metrics: transport::metrics::Registry,
) -> impl ServeConnection<Connection>
where
    R: Resolve<NameAddr, Endpoint = Metadata> + Clone + Send + Sync + 'static,
    R::Future: futures::Future<Error = Unresolvable> + Send,
    R::Resolution: Send,
    <R::Resolution as Resolution>::Error: std::error::Error + Send + Sync + 'static,
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

    // Establishes connections to remote peers (for both TCP
    // forwarding and HTTP proxying).
    let connect = svc::builder()
        .layer(transport_metrics.connect("outbound"))
        .timeout(config.outbound_connect_timeout)
        .layer(keepalive::connect::layer(config.outbound_connect_keepalive))
        .layer(tls::client::layer(local_identity))
        .service(connect::svc());

    // Instantiates an HTTP client for for a `client::Config`
    let client_stack = svc::builder()
        .layer(normalize_uri::layer())
        .layer(reconnect::layer().with_backoff(config.outbound_connect_backoff.clone()))
        .layer(client::layer("out", config.h2_settings))
        .service(connect.clone());

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
    let endpoint_stack = svc::builder()
        .layer(require_identity_on_endpoint::layer())
        .layer(http_metrics::layer::<_, classify::Response>(
            endpoint_http_metrics,
        ))
        .layer(tap_layer.clone())
        .layer(orig_proto_upgrade::layer())
        // disabled on purpose
        //.layer(add_server_id_on_rsp::layer())
        //.layer(add_remote_ip_on_rsp::layer())
        .layer(strip_header::request::layer(super::L5D_REQUIRE_ID))
        .layer(strip_header::response::layer(super::L5D_SERVER_ID))
        .layer(strip_header::response::layer(super::L5D_REMOTE_IP))
        .service(client_stack);

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
    let dst_route_layer = svc::builder()
        .buffer_pending(max_in_flight, DispatchDeadline::extract)
        .layer(classify::layer())
        .layer(http_metrics::layer::<_, classify::Response>(
            route_http_metrics,
        ))
        .layer(proxy::http::timeout::layer())
        .layer(retry::layer(retry_http_metrics.clone()))
        .layer(http_metrics::layer::<_, classify::Response>(
            retry_http_metrics,
        ))
        .layer(insert::target::layer());

    // Routes requests to their original destination endpoints. Used as
    // a fallback when service discovery has no endpoints for a destination.
    //
    // If the `l5d-require-id` header is present, then that identity is
    // used as the server name when connecting to the endpoint.
    let orig_dst_router = svc::builder()
        .layer(router::layer(
            router::Config::new("out ep", capacity, max_idle_age),
            |req: &http::Request<_>| {
                let ep = Endpoint::from_request(req);
                debug!("outbound ep={:?}", ep);
                ep
            },
        ))
        .buffer_pending(max_in_flight, DispatchDeadline::extract);

    // Resolves the target via the control plane and balances requests
    // over all endpoints returned from the destination service.
    let balancer = svc::builder()
        .layer(balance::layer(EWMA_DEFAULT_RTT, EWMA_DECAY))
        .layer(resolve::layer(discovery::Resolve::new(resolve)))
        .spawn_ready();

    let distributor = svc::builder()
        .layer(
            // Attempt to build a balancer. If the service is
            // unresolvable, fall back to using a router that dispatches
            // request to the application-selected original destination.
            fallback::layer(balancer, orig_dst_router).on_error::<Unresolvable>(),
        )
        .service(endpoint_stack);

    // A per-`DstAddr` stack that does the following:
    //
    // 1. Adds the `CANONICAL_DST_HEADER` from the `DstAddr`.
    // 2. Determines the profile of the destination and applies
    //    per-route policy.
    // 3. Creates a load balancer , configured by resolving the
    //   `DstAddr` with a resolver.
    let dst_stack = svc::builder()
        .layer(header_from_target::layer(super::CANONICAL_DST_HEADER))
        .layer(profiles::router::layer(
            profile_suffixes,
            profiles_client,
            dst_route_layer,
        ))
        .buffer_pending(max_in_flight, DispatchDeadline::extract)
        .service(distributor);

    // Routes request using the `DstAddr` extension.
    //
    // This is shared across addr-stacks so that multiple addrs that
    // canonicalize to the same DstAddr use the same dst-stack service.
    let dst_router = svc::builder()
        .layer(router::layer(
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
        .buffer_pending(max_in_flight, DispatchDeadline::extract)
        .service(dst_stack)
        .make();

    // Canonicalizes the request-specified `Addr` via DNS, and
    // annotates each request with a refined `Addr` so that it may be
    // routed by the dst_router.
    let addr_stack = svc::builder()
        .layer(canonicalize::layer(dns_resolver, canonicalize_timeout))
        .service(svc::shared(dst_router));

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
    let addr_router = svc::builder()
        .layer(router::layer(
            router::Config::new("out addr", capacity, max_idle_age),
            |req: &http::Request<_>| {
                super::http_request_l5d_override_dst_addr(req)
                    .map(|override_addr| {
                        debug!("outbound addr={:?}; dst-override", override_addr);
                        override_addr
                    })
                    .or_else(|_| {
                        let addr = super::http_request_authority_addr(req)
                            .or_else(|_| super::http_request_host_addr(req))
                            .or_else(|_| super::http_request_orig_dst_addr(req));
                        debug!("outbound addr={:?}", addr);
                        addr
                    })
                    .ok()
            },
        ))
        .buffer_pending(max_in_flight, DispatchDeadline::extract)
        .layer(insert::target::layer())
        .layer(strip_header::request::layer(super::DST_OVERRIDE_HEADER))
        .layer(strip_header::request::layer(super::L5D_CLIENT_ID))
        .service(addr_stack)
        .make();

    // Share a single semaphore across all requests to signal when
    // the proxy is overloaded.
    let admission_control = svc::builder()
        .load_shed()
        .concurrency_limit(max_in_flight)
        .service(addr_router);

    // Instantiates an HTTP service for each `Source` using the
    // shared `addr_router`. The `Source` is stored in the request's
    // extensions so that it can be used by the `addr_router`.
    let server_stack = svc::builder()
        .layer(handle_time.layer())
        .layer(super::errors::layer())
        .layer(insert::target::layer())
        .layer(insert::layer(move || {
            DispatchDeadline::after(dispatch_timeout)
        }))
        .service(svc::shared(admission_control));

    // Instantiated for each TCP connection received from the local
    // application (including HTTP connections).
    let accept = accept::builder()
        .layer(transport_metrics.accept("outbound"))
        .layer(keepalive::accept::layer(config.outbound_accept_keepalive));

    Server::new(
        "out",
        local_addr,
        accept,
        connect,
        server_stack,
        config.h2_settings,
    )
}
