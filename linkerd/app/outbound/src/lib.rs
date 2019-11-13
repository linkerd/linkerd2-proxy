//! Configures and runs the outbound proxy.
//!
//! The outound proxy is responsible for routing traffic from the local
//! application to external network endpoints.

#![deny(warnings, rust_2018_idioms)]

use futures::future;
use linkerd2_app_core::{
    self as core, classify,
    config::{ProxyConfig, ServerConfig},
    dns, drain,
    dst::DstAddr,
    errors, http_request_authority_addr, http_request_host_addr,
    http_request_l5d_override_dst_addr, http_request_orig_dst_addr,
    opencensus::proto::trace::v1 as oc,
    proxy::{
        self, core::resolve::Resolve, discover, fallback, http, identity, resolve::map_endpoint,
        tap, tcp, Server,
    },
    reconnect, router, serve,
    spans::SpanConverter,
    svc, trace, trace_context,
    transport::{self, connect, tls, OrigDstAddr, SysOrigDstAddr},
    Addr, Conditional, DispatchDeadline, Error, ProxyMetrics, CANONICAL_DST_HEADER,
    DST_OVERRIDE_HEADER, L5D_CLIENT_ID, L5D_REMOTE_IP, L5D_REQUIRE_ID, L5D_SERVER_ID,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::mpsc;
use tower_grpc::{self as grpc, generic::client::GrpcService};
use tracing::{debug, info_span};

#[allow(dead_code)] // TODO #2597
mod add_remote_ip_on_rsp;
#[allow(dead_code)] // TODO #2597
mod add_server_id_on_rsp;
mod endpoint;
mod orig_proto_upgrade;
mod require_identity_on_endpoint;

pub use self::endpoint::Endpoint;

const EWMA_DEFAULT_RTT: Duration = Duration::from_millis(30);
const EWMA_DECAY: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct Config<A: OrigDstAddr = SysOrigDstAddr> {
    pub proxy: ProxyConfig<A>,
    pub canonicalize_timeout: Duration,
}

pub struct Outbound {
    pub listen_addr: SocketAddr,
    pub serve: serve::Task,
}

impl<A: OrigDstAddr> Config<A> {
    pub fn with_orig_dst_addr<B: OrigDstAddr>(self, orig_dst_addr: B) -> Config<B> {
        Config {
            proxy: self.proxy.with_orig_dst_addr(orig_dst_addr),
            canonicalize_timeout: self.canonicalize_timeout,
        }
    }

    pub fn build<R, P>(
        self,
        local_identity: tls::Conditional<identity::Local>,
        resolve: R,
        dns_resolver: dns::Resolver,
        profiles_client: core::profiles::Client<P>,
        tap_layer: tap::Layer,
        metrics: ProxyMetrics,
        span_sink: Option<mpsc::Sender<oc::Span>>,
        drain: drain::Watch,
    ) -> Result<Outbound, Error>
    where
        A: Send + 'static,
        R: Resolve<DstAddr, Endpoint = proxy::api_resolve::Metadata>
            + Clone
            + Send
            + Sync
            + 'static,
        R::Future: Send,
        R::Resolution: Send,
        P: GrpcService<grpc::BoxBody> + Clone + Send + Sync + 'static,
        P::ResponseBody: Send,
        <P::ResponseBody as grpc::Body>::Data: Send,
        P::Future: Send,
    {
        use proxy::core::listen::{Bind, Listen};
        let Config {
            canonicalize_timeout,
            proxy:
                ProxyConfig {
                    server:
                        ServerConfig {
                            bind,
                            buffer,
                            h2_settings,
                        },
                    connect,
                    router_capacity,
                    router_max_idle_age,
                    disable_protocol_detection_for_ports,
                },
        } = self;

        let listen = bind.bind().map_err(Error::from)?;
        let listen_addr = listen.listen_addr();

        // The stack is served lazily since some layers (notably buffer) spawn
        // tasks from their constructor. This helps to ensure that tasks are
        // spawned on the same runtime as the proxy.
        let serve = Box::new(future::lazy(move || {
            // Establishes connections to remote peers (for both TCP
            // forwarding and HTTP proxying).
            let connect_stack = svc::stack(connect::svc(connect.keepalive))
                .push(tls::client::layer(local_identity))
                .push_timeout(connect.timeout)
                .push(metrics.transport.layer_connect(TransportLabels));

            // Instantiates an HTTP client for for a `client::Config`
            let client_stack = connect_stack
                .clone()
                .push(http::client::layer(connect.h2_settings))
                .push(reconnect::layer({
                    let backoff = connect.backoff.clone();
                    move |_| Ok(backoff.stream())
                }))
                .push(trace_context::layer(span_sink.clone().map(|span_sink| {
                    SpanConverter::client(span_sink, trace_labels())
                })))
                .push(http::normalize_uri::layer());

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
                .serves::<Endpoint>()
                .push(http::strip_header::response::layer(L5D_REMOTE_IP))
                .push(http::strip_header::response::layer(L5D_SERVER_ID))
                .push(http::strip_header::request::layer(L5D_REQUIRE_ID))
                // disabled due to information leagkage
                //.push(add_remote_ip_on_rsp::layer())
                //.push(add_server_id_on_rsp::layer())
                .push(orig_proto_upgrade::layer())
                .push(tap_layer.clone())
                .push(http::metrics::layer::<_, classify::Response>(
                    metrics.http_endpoint,
                ))
                .push(require_identity_on_endpoint::layer())
                .push(trace::layer(|endpoint: &Endpoint| {
                    info_span!("endpoint", peer.addr = %endpoint.addr, peer.id = ?endpoint.identity)
                }))
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
                .push(http::insert::target::layer())
                .push(http::metrics::layer::<_, classify::Response>(
                    metrics.http_route_retry.clone(),
                ))
                .push(http::retry::layer(metrics.http_route_retry))
                .push(http::timeout::layer())
                .push(http::metrics::layer::<_, classify::Response>(
                    metrics.http_route,
                ))
                .push(classify::layer())
                .push_buffer_pending(buffer.max_in_flight, DispatchDeadline::extract);

            // Routes requests to their original destination endpoints. Used as
            // a fallback when service discovery has no endpoints for a destination.
            //
            // If the `l5d-require-id` header is present, then that identity is
            // used as the server name when connecting to the endpoint.
            let orig_dst_router_layer = svc::layers()
                .push_buffer_pending(buffer.max_in_flight, DispatchDeadline::extract)
                .push(router::Layer::new(
                    router::Config::new(router_capacity, router_max_idle_age),
                    Endpoint::from_request,
                ));

            // Resolves the target via the control plane and balances requests
            // over all endpoints returned from the destination service.
            const DISCOVER_UPDATE_BUFFER_CAPACITY: usize = 2;
            let balancer_layer = svc::layers()
                .push_spawn_ready()
                .push(discover::Layer::new(
                    DISCOVER_UPDATE_BUFFER_CAPACITY,
                    map_endpoint::Resolve::new(endpoint::FromMetadata, resolve.clone()),
                ))
                .push(http::balance::layer(EWMA_DEFAULT_RTT, EWMA_DECAY));

            // If the balancer fails to be created, i.e., because it is unresolvable,
            // fall back to using a router that dispatches request to the
            // application-selected original destination.
            let distributor = endpoint_stack
                .serves::<Endpoint>()
                .push(fallback::layer(
                    balancer_layer.boxed(),
                    orig_dst_router_layer.boxed(),
                ))
                .push(trace::layer(
                    |dst: &DstAddr| info_span!("concrete", dst.concrete = %dst.dst_concrete()),
                ));

            // A per-`DstAddr` stack that does the following:
            //
            // 1. Adds the `CANONICAL_DST_HEADER` from the `DstAddr`.
            // 2. Determines the profile of the destination and applies
            //    per-route policy.
            // 3. Creates a load balancer , configured by resolving the
            //   `DstAddr` with a resolver.
            let dst_stack = distributor
                .serves::<DstAddr>()
                .push_buffer_pending(buffer.max_in_flight, DispatchDeadline::extract)
                .makes::<DstAddr>()
                .push(http::profiles::router::layer(
                    profiles_client,
                    dst_route_layer,
                ))
                .push(http::header_from_target::layer(CANONICAL_DST_HEADER));

            // Routes request using the `DstAddr` extension.
            //
            // This is shared across addr-stacks so that multiple addrs that
            // canonicalize to the same DstAddr use the same dst-stack service.
            let dst_router = dst_stack
                .push(trace::layer(
                    |dst: &DstAddr| info_span!("logical", dst.logical = %dst.dst_logical()),
                ))
                .push_buffer_pending(buffer.max_in_flight, DispatchDeadline::extract)
                .push(router::Layer::new(
                    router::Config::new(router_capacity, router_max_idle_age),
                    |req: &http::Request<_>| {
                        req.extensions().get::<Addr>().cloned().map(|addr| {
                            DstAddr::outbound(addr, http::settings::Settings::from_request(req))
                        })
                    },
                ))
                .into_inner()
                .make();

            // Canonicalizes the request-specified `Addr` via DNS, and
            // annotates each request with a refined `Addr` so that it may be
            // routed by the dst_router.
            let addr_stack = svc::stack(svc::Shared::new(dst_router)).push(
                http::canonicalize::layer(dns_resolver, canonicalize_timeout),
            );

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
            // 5. Finally, if the tls::accept::Meta had an SO_ORIGINAL_DST, this TCP
            // address is used.
            let addr_router = addr_stack
                .push(http::strip_header::request::layer(L5D_CLIENT_ID))
                .push(http::strip_header::request::layer(DST_OVERRIDE_HEADER))
                .push(http::insert::target::layer())
                .push(trace::layer(|addr: &Addr| info_span!("addr", %addr)))
                .push_buffer_pending(buffer.max_in_flight, DispatchDeadline::extract)
                .push(router::Layer::new(
                    router::Config::new(router_capacity, router_max_idle_age),
                    |req: &http::Request<_>| {
                        http_request_l5d_override_dst_addr(req)
                            .map(|override_addr| {
                                debug!("using dst-override");
                                override_addr
                            })
                            .or_else(|_| http_request_authority_addr(req))
                            .or_else(|_| http_request_host_addr(req))
                            .or_else(|_| http_request_orig_dst_addr(req))
                            .ok()
                    },
                ))
                .into_inner()
                .make();

            // Share a single semaphore across all requests to signal when
            // the proxy is overloaded.
            let admission_control = svc::stack(addr_router)
                .push_concurrency_limit(buffer.max_in_flight)
                .push_load_shed();

            // Instantiates an HTTP service for each `tls::accept::Meta` using the
            // shared `addr_router`. The `tls::accept::Meta` is stored in the request's
            // extensions so that it can be used by the `addr_router`.
            let server_stack = svc::stack(svc::Shared::new(admission_control))
                .push(http::insert::layer(move || {
                    DispatchDeadline::after(buffer.dispatch_timeout)
                }))
                .push(http::insert::target::layer())
                .push(errors::layer())
                .push(trace::layer(
                    |src: &tls::accept::Meta| info_span!("source", target.addr = %src.addrs.target_addr()),
                ))
                .push(trace_context::layer(span_sink.map(|span_sink| {
                    SpanConverter::server(span_sink, trace_labels())
                })))
                .push(metrics.http_handle_time.layer());

            let forward_tcp = tcp::Forward::new(
                svc::stack(connect_stack)
                    .push(svc::map_target::layer(|meta: tls::accept::Meta| {
                        Endpoint::from(meta.addrs.target_addr())
                    }))
                    .into_inner(),
            );

            let proxy = Server::new(
                TransportLabels,
                metrics.transport,
                forward_tcp,
                server_stack,
                h2_settings,
                drain.clone(),
                disable_protocol_detection_for_ports.clone(),
            );

            let no_tls: tls::Conditional<identity::Local> =
                Conditional::None(tls::ReasonForNoPeerName::Loopback.into());
            let accept = tls::AcceptTls::new(no_tls, proxy)
                .with_skip_ports(disable_protocol_detection_for_ports);

            serve::serve(listen, accept, drain)
        }));

        Ok(Outbound { listen_addr, serve })
    }
}

#[derive(Copy, Clone, Debug)]
struct TransportLabels;

impl transport::metrics::TransportLabels<Endpoint> for TransportLabels {
    type Labels = transport::labels::Key;

    fn transport_labels(&self, endpoint: &Endpoint) -> Self::Labels {
        transport::labels::Key::connect("outbound", endpoint.identity.as_ref())
    }
}

impl transport::metrics::TransportLabels<proxy::server::Protocol> for TransportLabels {
    type Labels = transport::labels::Key;

    fn transport_labels(&self, proto: &proxy::server::Protocol) -> Self::Labels {
        transport::labels::Key::accept("outbound", proto.tls.peer_identity.as_ref())
    }
}

pub fn trace_labels() -> HashMap<String, String> {
    let mut l = HashMap::new();
    l.insert("direction".to_string(), "outbound".to_string());
    l
}
