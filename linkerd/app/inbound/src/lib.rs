//! Configures and runs the inbound proxy.
//!
//! The inbound proxy is responsible for terminating traffic from other network
//! endpoints inbound to the local application.

#![deny(warnings, rust_2018_idioms)]

use linkerd2_app_core::{
    classify,
    config::Config,
    drain,
    dst::DstAddr,
    errors, http_request_authority_addr, http_request_host_addr,
    http_request_l5d_override_dst_addr, http_request_orig_dst_addr, identity,
    proxy::{
        http::{
            client, insert, metrics as http_metrics, normalize_uri, profiles, router, settings,
            strip_header,
        },
        server::{Protocol as ServerProtocol, Server},
    },
    reconnect, serve,
    spans::SpanConverter,
    svc, trace, trace_context,
    transport::{self, connect, tls, Listen, OrigDstAddr},
    Addr, DispatchDeadline, CANONICAL_DST_HEADER, DST_OVERRIDE_HEADER, L5D_CLIENT_ID,
    L5D_REMOTE_IP, L5D_SERVER_ID,
};
use opencensus_proto::trace::v1 as oc;
use std::collections::HashMap;
use tokio::sync::mpsc;
use tower_grpc::{self as grpc, generic::client::GrpcService};
use tracing::{debug, info_span};

mod endpoint;
mod orig_proto_downgrade;
mod rewrite_loopback_addr;
#[allow(dead_code)] // TODO #2597
mod set_client_id_on_req;
#[allow(dead_code)] // TODO #2597
mod set_remote_ip_on_req;

pub use self::endpoint::{Endpoint, RecognizeEndpoint};

pub fn spawn<A, P>(
    config: &Config,
    local_identity: tls::Conditional<identity::Local>,
    listen: Listen<A>,
    profiles_client: linkerd2_app_core::profiles::Client<P>,
    tap_layer: linkerd2_app_core::tap::Layer,
    handle_time: http_metrics::handle_time::Scope,
    endpoint_http_metrics: linkerd2_app_core::HttpEndpointMetricsRegistry,
    route_http_metrics: linkerd2_app_core::HttpRouteMetricsRegistry,
    transport_metrics: linkerd2_app_core::transport::MetricsRegistry,
    span_sink: Option<mpsc::Sender<oc::Span>>,
    drain: drain::Watch,
) where
    A: OrigDstAddr + Send + 'static,
    P: GrpcService<grpc::BoxBody> + Clone + Send + Sync + 'static,
    P::ResponseBody: Send,
    <P::ResponseBody as grpc::Body>::Data: Send,
    P::Future: Send,
{
    let capacity = config.inbound_router_capacity;
    let max_idle_age = config.inbound_router_max_idle_age;
    let max_in_flight = config.inbound_max_requests_in_flight;
    let profile_suffixes = config.destination_profile_suffixes.clone();
    let dispatch_timeout = config.inbound_dispatch_timeout;

    let mut trace_labels = HashMap::new();
    trace_labels.insert("direction".to_string(), "inbound".to_string());

    // Establishes connections to the local application (for both
    // TCP forwarding and HTTP proxying).
    let connect = svc::stack(connect::svc(config.inbound_connect_keepalive))
        .push(tls::client::layer(local_identity.clone()))
        .push_timeout(config.inbound_connect_timeout)
        .push(rewrite_loopback_addr::layer());

    let trace_context_layer = trace_context::layer(
        span_sink
            .clone()
            .map(|span_sink| SpanConverter::client(span_sink, trace_labels.clone())),
    );
    // Instantiates an HTTP client for a `client::Config`
    let client_stack = connect
        .clone()
        .push(client::layer(config.h2_settings))
        .push(reconnect::layer({
            let backoff = config.inbound_connect_backoff.clone();
            move |_| Ok(backoff.stream())
        }))
        .push(trace_context_layer)
        .push(normalize_uri::layer());

    // A stack configured by `router::Config`, responsible for building
    // a router made of route stacks configured by `inbound::Endpoint`.
    let endpoint_router = client_stack
        .push(tap_layer)
        .push(http_metrics::layer::<_, classify::Response>(
            endpoint_http_metrics,
        ))
        .serves::<Endpoint>()
        .push(trace::layer(|endpoint: &Endpoint| {
            info_span!("endpoint", ?endpoint)
        }))
        .push_buffer_pending(max_in_flight, DispatchDeadline::extract)
        .makes::<Endpoint>()
        .push(router::layer(
            router::Config::new(capacity, max_idle_age),
            RecognizeEndpoint::default(),
        ))
        .into_inner()
        .make();

    // A per-`dst::Route` layer that uses profile data to configure
    // a per-route layer.
    //
    // The `classify` module installs a `classify::Response`
    // extension into each request so that all lower metrics
    // implementations can use the route-specific configuration.
    let dst_route_layer = svc::layers()
        .push(insert::target::layer())
        .push(http_metrics::layer::<_, classify::Response>(
            route_http_metrics,
        ))
        .push(classify::layer())
        .push_buffer_pending(max_in_flight, DispatchDeadline::extract);

    // A per-`DstAddr` stack that does the following:
    //
    // 1. Determines the profile of the destination and applies
    //    per-route policy.
    // 2. Annotates the request with the `DstAddr` so that
    //    `RecognizeEndpoint` can use the value.
    let dst_stack = svc::stack(svc::Shared::new(endpoint_router))
        .push(insert::target::layer())
        .push_buffer_pending(max_in_flight, DispatchDeadline::extract)
        .push(profiles::router::layer(
            profile_suffixes,
            profiles_client,
            dst_route_layer,
        ))
        .push(strip_header::request::layer(DST_OVERRIDE_HEADER))
        .push(trace::layer(
            |dst: &DstAddr| info_span!("logical", dst = %dst.dst_logical()),
        ));

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
    // 6. Finally, if the tls::accept::Meta had an SO_ORIGINAL_DST, this TCP
    // address is used.
    let dst_router = dst_stack
        .push_buffer_pending(max_in_flight, DispatchDeadline::extract)
        .push(router::layer(
            router::Config::new(capacity, max_idle_age),
            |req: &http::Request<_>| {
                let dst = req
                    .headers()
                    .get(CANONICAL_DST_HEADER)
                    .and_then(|dst| {
                        dst.to_str().ok().and_then(|d| {
                            Addr::from_str(d).ok().map(|a| {
                                debug!("using {}", CANONICAL_DST_HEADER);
                                a
                            })
                        })
                    })
                    .or_else(|| {
                        http_request_l5d_override_dst_addr(req)
                            .ok()
                            .map(|override_addr| {
                                debug!("using {}", DST_OVERRIDE_HEADER);
                                override_addr
                            })
                    })
                    .or_else(|| http_request_authority_addr(req).ok())
                    .or_else(|| http_request_host_addr(req).ok())
                    .or_else(|| http_request_orig_dst_addr(req).ok())
                    .map(|addr| DstAddr::inbound(addr, settings::Settings::from_request(req)));
                debug!(dst.logical = ?dst);
                dst
            },
        ))
        .into_inner()
        .make();

    // Share a single semaphore across all requests to signal when
    // the proxy is overloaded.
    let admission_control = svc::stack(dst_router)
        .push_concurrency_limit(max_in_flight)
        .push_load_shed();

    let trace_context_layer = trace_context::layer(
        span_sink.map(|span_sink| SpanConverter::server(span_sink, trace_labels)),
    );
    // As HTTP requests are accepted, the `tls::accept::Meta` connection
    // metadata is stored on each request's extensions.
    //
    // Furthermore, HTTP/2 requests may be downgraded to HTTP/1.1 per
    // `orig-proto` headers. This happens in the source stack so that
    // the router need not detect whether a request _will be_ downgraded.
    let source_stack = svc::stack(svc::Shared::new(admission_control))
        .serves::<tls::accept::Meta>()
        .push(orig_proto_downgrade::layer())
        .push(insert::target::layer())
        // disabled due to information leagkage
        //.push(set_remote_ip_on_req::layer())
        //.push(set_client_id_on_req::layer())
        .push(strip_header::request::layer(L5D_REMOTE_IP))
        .push(strip_header::request::layer(L5D_CLIENT_ID))
        .push(strip_header::response::layer(L5D_SERVER_ID))
        .push(insert::layer(move || {
            DispatchDeadline::after(dispatch_timeout)
        }))
        .push(errors::layer())
        .push(trace::layer(
            |src: &tls::accept::Meta| info_span!("source", peer_ip = %src.addrs.peer().ip(), peer_id = ?src.peer_identity),
        ))
        .push(trace_context_layer)
        .push(handle_time.layer())
        .serves::<tls::accept::Meta>();

    let skip_ports = std::sync::Arc::new(config.inbound_ports_disable_protocol_detection.clone());
    let server = Server::new(
        TransportLabels,
        transport_metrics,
        svc::stack(connect)
            .push(svc::map_target::layer(Endpoint::from))
            .into_inner(),
        source_stack,
        config.h2_settings,
        drain.clone(),
        skip_ports.clone(),
    );

    let accept = tls::AcceptTls::new(local_identity, server).with_skip_ports(skip_ports);
    serve::spawn(listen, accept, drain);
}

#[derive(Copy, Clone, Debug)]
struct TransportLabels;

impl transport::metrics::TransportLabels<ServerProtocol> for TransportLabels {
    type Labels = transport::labels::Key;

    fn transport_labels(&self, proto: &ServerProtocol) -> Self::Labels {
        transport::labels::Key::accept(
            proto.tls.addrs.peer(),
            &proto.tls.peer_identity,
            proto.http.is_some(),
        )
    }
}
