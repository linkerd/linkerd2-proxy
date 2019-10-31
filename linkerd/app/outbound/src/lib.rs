//! Configures and runs the outbound proxy.
//!
//! The outound proxy is responsible for routing traffic from the local
//! application to external network endpoints.

#![deny(warnings, rust_2018_idioms)]

use futures::Future;
use linkerd2_app_core::{
    self as core, classify,
    config::ProxyConfig,
    dns, drain,
    dst::DstAddr,
    errors, http_request_authority_addr, http_request_host_addr,
    http_request_l5d_override_dst_addr, http_request_orig_dst_addr,
    proxy::{
        self,
        core::resolve::Resolve,
        discover,
        http::{
            balance, canonicalize, client, fallback, header_from_target, insert,
            metrics as http_metrics, normalize_uri, profiles, retry, router, settings,
            strip_header,
        },
        identity,
        resolve::map_endpoint,
        tap, Server,
    },
    reconnect, serve,
    spans::SpanConverter,
    svc, trace, trace_context,
    transport::{self, connect, tls, OrigDstAddr, SysOrigDstAddr},
    Addr, Conditional, DispatchDeadline, Error, ProxyMetrics, CANONICAL_DST_HEADER,
    DST_OVERRIDE_HEADER, L5D_CLIENT_ID, L5D_REMOTE_IP, L5D_REQUIRE_ID, L5D_SERVER_ID,
};
use opencensus_proto::trace::v1 as oc;
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
    pub purge_dsts: PurgeTask,
    pub purge_addrs: PurgeTask,
}

pub type PurgeTask = Box<dyn Future<Item = (), Error = ()> + Send + 'static>;

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
        let listen = self.proxy.server.bind.bind().map_err(Error::from)?;
        let listen_addr = listen.listen_addr();

        let capacity = self.proxy.router_capacity;
        let max_idle_age = self.proxy.router_max_idle_age;
        let max_in_flight = self.proxy.server.buffer.max_in_flight;
        let dispatch_timeout = self.proxy.server.buffer.dispatch_timeout;
        let canonicalize_timeout = self.canonicalize_timeout;

        let mut trace_labels = HashMap::new();
        trace_labels.insert("direction".to_string(), "outbound".to_string());

        // Establishes connections to remote peers (for both TCP
        // forwarding and HTTP proxying).
        let connect = svc::stack(connect::svc(self.proxy.connect.keepalive))
            .push(tls::client::layer(local_identity))
            .push_timeout(self.proxy.connect.timeout)
            .push(metrics.transport.layer_connect(TransportLabels));

        let trace_context_layer = trace_context::layer(
            span_sink
                .clone()
                .map(|span_sink| SpanConverter::client(span_sink, trace_labels.clone())),
        );
        // Instantiates an HTTP client for for a `client::Config`
        let client_stack = connect
            .clone()
            .push(client::layer(self.proxy.connect.h2_settings))
            .push(reconnect::layer({
                let backoff = self.proxy.connect.backoff.clone();
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
        .serves::<Endpoint>()
        .push(strip_header::response::layer(L5D_REMOTE_IP))
        .push(strip_header::response::layer(L5D_SERVER_ID))
        .push(strip_header::request::layer(L5D_REQUIRE_ID))
        // disabled due to information leagkage
        //.push(add_remote_ip_on_rsp::layer())
        //.push(add_server_id_on_rsp::layer())
        .push(orig_proto_upgrade::layer())
        .push(tap_layer.clone())
        .push(http_metrics::layer::<_, classify::Response>(
            metrics.http_endpoint,
        ))
        .push(require_identity_on_endpoint::layer())
        .push(trace::layer(|endpoint: &Endpoint| {
            info_span!("endpoint", peer.addr = %endpoint.addr, peer.id = ?endpoint.identity)
        }));

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
                metrics.http_route_retry.clone(),
            ))
            .push(retry::layer(metrics.http_route_retry))
            .push(proxy::http::timeout::layer())
            .push(http_metrics::layer::<_, classify::Response>(
                metrics.http_route,
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
                router::Config::new(capacity, max_idle_age),
                Endpoint::from_request,
            ));

        // Resolves the target via the control plane and balances requests
        // over all endpoints returned from the destination service.
        const DISCOVER_UPDATE_BUFFER_CAPACITY: usize = 2;
        let balancer_layer = svc::layers()
            .push_spawn_ready()
            .push(discover::Layer::new(
                DISCOVER_UPDATE_BUFFER_CAPACITY,
                map_endpoint::Resolve::new(endpoint::FromMetadata, resolve),
            ))
            .push(balance::layer(EWMA_DEFAULT_RTT, EWMA_DECAY));

        // If the balancer fails to be created, i.e., because it is unresolvable,
        // fall back to using a router that dispatches request to the
        // application-selected original destination.
        let distributor = endpoint_stack
            .push(fallback::layer(balancer_layer, orig_dst_router_layer))
            .serves::<DstAddr>()
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
            .push_buffer_pending(max_in_flight, DispatchDeadline::extract)
            .push(profiles::router::layer(profiles_client, dst_route_layer))
            .push(header_from_target::layer(CANONICAL_DST_HEADER));

        // Routes request using the `DstAddr` extension.
        //
        // This is shared across addr-stacks so that multiple addrs that
        // canonicalize to the same DstAddr use the same dst-stack service.
        let (dst_router, purge_dsts) = dst_stack
            .push(trace::layer(
                |dst: &DstAddr| info_span!("logical", dst.logical = %dst.dst_logical()),
            ))
            .push_buffer_pending(max_in_flight, DispatchDeadline::extract)
            .push(router::layer(
                router::Config::new(capacity, max_idle_age),
                |req: &http::Request<_>| {
                    req.extensions()
                        .get::<Addr>()
                        .cloned()
                        .map(|addr| DstAddr::outbound(addr, settings::Settings::from_request(req)))
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
        // 5. Finally, if the tls::accept::Meta had an SO_ORIGINAL_DST, this TCP
        // address is used.
        let (addr_router, purge_addrs) = addr_stack
            .push(strip_header::request::layer(L5D_CLIENT_ID))
            .push(strip_header::request::layer(DST_OVERRIDE_HEADER))
            .push(insert::target::layer())
            .push(trace::layer(|addr: &Addr| info_span!("addr", %addr)))
            .push_buffer_pending(max_in_flight, DispatchDeadline::extract)
            .push(router::layer(
                router::Config::new(capacity, max_idle_age),
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
            .push_concurrency_limit(max_in_flight)
            .push_load_shed();

        let trace_context_layer = trace_context::layer(
            span_sink.map(|span_sink| SpanConverter::server(span_sink, trace_labels)),
        );
        // Instantiates an HTTP service for each `tls::accept::Meta` using the
        // shared `addr_router`. The `tls::accept::Meta` is stored in the request's
        // extensions so that it can be used by the `addr_router`.
        let server_stack = svc::stack(svc::Shared::new(admission_control))
        .push(insert::layer(move || {
            DispatchDeadline::after(dispatch_timeout)
        }))
        .push(insert::target::layer())
        .push(errors::layer())
        .push(trace::layer(
            |src: &tls::accept::Meta| info_span!("source", target.addr = %src.addrs.target_addr()),
        ))
        .push(trace_context_layer)
        .push(metrics.http_handle_time.layer());

        let skip_ports = self.proxy.disable_protocol_detection_for_ports;

        let proxy = Server::new(
            TransportLabels,
            metrics.transport,
            svc::stack(connect)
                .push(svc::map_target::layer(Endpoint::from))
                .into_inner(),
            server_stack,
            self.proxy.server.h2_settings,
            drain.clone(),
            skip_ports.clone(),
        );

        let no_tls: tls::Conditional<identity::Local> =
            Conditional::None(tls::ReasonForNoPeerName::Loopback.into());
        let accept = tls::AcceptTls::new(no_tls, proxy).with_skip_ports(skip_ports);

        let serve = serve::serve(listen, accept, drain);

        Ok(Outbound {
            listen_addr,
            serve,
            purge_dsts: Box::new(purge_dsts),
            purge_addrs: Box::new(purge_addrs),
        })
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
