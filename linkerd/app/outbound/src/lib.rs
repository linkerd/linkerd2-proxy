//! Configures and runs the outbound proxy.
//!
//! The outound proxy is responsible for routing traffic from the local
//! application to external network endpoints.

#![deny(warnings, rust_2018_idioms)]

pub use self::endpoint::{HttpConcrete, HttpEndpoint, HttpLogical, LogicalPerRequest, TcpEndpoint};
use ::http::header::HOST;
use futures::{future, prelude::*};
use linkerd2_app_core::{
    admit, classify,
    config::{ProxyConfig, ServerConfig},
    dns, drain, errors, metric_labels,
    opencensus::proto::trace::v1 as oc,
    profiles,
    proxy::{
        self, core::resolve::Resolve, discover, http, identity, resolve::map_endpoint, tap, tcp,
    },
    reconnect, retry, router, serve,
    spans::SpanConverter,
    svc::{self, NewService},
    transport::{self, listen, tls},
    Addr, Conditional, DiscoveryRejected, Error, ProxyMetrics, StackMetrics, TraceContextLayer,
    CANONICAL_DST_HEADER, DST_OVERRIDE_HEADER, L5D_REQUIRE_ID,
};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use tokio::sync::mpsc;
use tracing::{info, info_span};

pub mod endpoint;
mod prevent_loop;
mod require_identity_on_endpoint;
#[cfg(test)]
mod tests;

use self::prevent_loop::PreventLoop;
use self::require_identity_on_endpoint::MakeRequireIdentityLayer;

const EWMA_DEFAULT_RTT: Duration = Duration::from_millis(30);
const EWMA_DECAY: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct Config {
    pub proxy: ProxyConfig,
    pub canonicalize_timeout: Duration,
}

impl Config {
    pub fn build_tcp_connect(
        &self,
        local_identity: tls::Conditional<identity::Local>,
        metrics: &ProxyMetrics,
    ) -> impl tower::Service<
        TcpEndpoint,
        Error = Error,
        Future = impl future::Future + Send,
        Response = impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    > + tower::Service<
        HttpEndpoint,
        Error = Error,
        Future = impl future::Future + Send,
        Response = impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    > + Unpin
           + Clone
           + Send {
        // Establishes connections to remote peers (for both TCP
        // forwarding and HTTP proxying).
        svc::connect(self.proxy.connect.keepalive)
            // Initiates mTLS if the target is configured with identity.
            .push(tls::client::ConnectLayer::new(local_identity))
            // Limits the time we wait for a connection to be established.
            .push_timeout(self.proxy.connect.timeout)
            .push(metrics.transport.layer_connect(TransportLabels))
            .into_inner()
    }

    pub fn build_dns_refine(
        &self,
        dns_resolver: dns::Resolver,
        metrics: &StackMetrics,
    ) -> impl tower::Service<
        dns::Name,
        Response = (dns::Name, IpAddr),
        Error = Error,
        Future = impl Unpin + Send,
    > + Unpin
           + Clone
           + Send {
        // Caches DNS refinements from relative names to canonical names.
        //
        // For example, a client may send requests to `foo` or `foo.ns`; and the canonical form
        // of these names is `foo.ns.svc.cluster.local
        svc::stack(dns_resolver.into_make_refine())
            .cache(
                svc::layers().push_on_response(
                    svc::layers()
                        // If the service has been unavailable for an extended time, eagerly
                        // fail requests.
                        .push_failfast(self.proxy.dispatch_timeout)
                        // Shares the service, ensuring discovery errors are propagated.
                        .push_spawn_buffer_with_idle_timeout(
                            self.proxy.buffer_capacity,
                            self.proxy.cache_max_idle_age,
                        )
                        .push(metrics.layer(stack_labels("refine"))),
                ),
            )
            .spawn_buffer(self.proxy.buffer_capacity)
            .instrument(|name: &dns::Name| info_span!("refine", %name))
            // Obtains the service, advances the state of the resolution
            .push(svc::make_response::Layer)
            .into_inner()
    }

    pub fn build_http_endpoint<B, C>(
        &self,
        prevent_loop: impl Into<PreventLoop>,
        tcp_connect: C,
        tap_layer: tap::Layer,
        metrics: ProxyMetrics,
        span_sink: Option<mpsc::Sender<oc::Span>>,
    ) -> impl tower::Service<
        HttpEndpoint,
        Error = Error,
        Future = impl Unpin + Send,
        Response = impl tower::Service<
            http::Request<B>,
            Response = http::Response<http::boxed::Payload>,
            Error = Error,
            Future = impl Send,
        > + Send,
    > + Unpin
           + Clone
           + Send
    where
        B: http::HttpBody<Error = Error> + std::fmt::Debug + Default + Send + 'static,
        B::Data: Send + 'static,
        C: tower::Service<HttpEndpoint, Error = Error> + Unpin + Clone + Send + Sync + 'static,
        C::Response: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
        C::Future: Unpin + Send,
    {
        // Registers the stack with Tap, Metrics, and OpenCensus tracing
        // export.
        let observability = svc::layers()
            .push(tap_layer.clone())
            .push(metrics.http_endpoint.into_layer::<classify::Response>())
            .push_on_response(TraceContextLayer::new(
                span_sink
                    .clone()
                    .map(|sink| SpanConverter::client(sink, trace_labels())),
            ));

        // Checks the headers to validate that a client-specified required
        // identity matches the configured identity.
        let identity_headers = svc::layers()
            .push_on_response(http::strip_header::request::layer(L5D_REQUIRE_ID))
            .push(MakeRequireIdentityLayer::new());

        svc::stack(tcp_connect)
            // Initiates an HTTP client on the underlying transport. Prior-knowledge HTTP/2
            // is typically used (i.e. when communicating with other proxies); though
            // HTTP/1.x fallback is supported as needed.
            .push(http::client::layer(self.proxy.connect.h2_settings))
            // Re-establishes a connection when the client fails.
            .push(reconnect::layer({
                let backoff = self.proxy.connect.backoff.clone();
                move |_| Ok(backoff.stream())
            }))
            .push(admit::AdmitLayer::new(prevent_loop.into()))
            .push(observability.clone())
            .push(identity_headers.clone())
            .push(http::override_authority::Layer::new(vec![
                HOST.as_str(),
                CANONICAL_DST_HEADER,
            ]))
            .push_on_response(svc::layers().box_http_response())
            .check_service::<HttpEndpoint>()
            .instrument(|e: &HttpEndpoint| info_span!("endpoint", peer.addr = %e.addr))
    }

    pub fn build_http_router<B, E, S, R, P>(
        &self,
        endpoint: E,
        resolve: R,
        profiles_client: P,
        metrics: ProxyMetrics,
    ) -> impl tower::Service<
        HttpLogical,
        Error = Error,
        Future = impl Unpin + Send,
        Response = impl tower::Service<
            http::Request<B>,
            Response = http::Response<http::boxed::Payload>,
            Error = Error,
            Future = impl Send,
        > + Send,
    > + Unpin
           + Clone
           + Send
    where
        B: http::HttpBody<Error = Error> + std::fmt::Debug + Default + Send + 'static,
        B::Data: Send + 'static,
        E: tower::Service<HttpEndpoint, Error = Error, Response = S>
            + Unpin
            + Clone
            + Send
            + Sync
            + 'static,
        E::Future: Unpin + Send,
        S: tower::Service<
                http::Request<http::boxed::Payload>,
                Response = http::Response<http::boxed::Payload>,
                Error = Error,
            > + Send
            + 'static,
        S::Future: Send,
        R: Resolve<HttpConcrete, Endpoint = proxy::api_resolve::Metadata>
            + Unpin
            + Clone
            + Send
            + 'static,
        R::Future: Unpin + Send,
        R::Resolution: Unpin + Send,
        P: profiles::GetProfile<HttpLogical> + Unpin + Clone + Send + 'static,
        P::Future: Unpin + Send,
        P::Error: Send,
    {
        let ProxyConfig {
            buffer_capacity,
            cache_max_idle_age,
            dispatch_timeout,
            ..
        } = self.proxy.clone();

        // Resolves each target via the control plane on a background task, buffering results.
        //
        // This buffer controls how many discovery updates may be pending/unconsumed by the
        // balancer before backpressure is applied on the resolution stream. If the buffer is
        // full for `cache_max_idle_age`, then the resolution task fails.
        let discover = svc::layers()
            .push(discover::resolve(map_endpoint::Resolve::new(
                endpoint::FromMetadata,
                resolve.clone(),
            )))
            .push(discover::buffer(1_000, cache_max_idle_age));

        // Builds a balancer for each concrete destination.
        let concrete = svc::stack(endpoint.clone())
            .check_make_service::<HttpEndpoint, http::Request<http::boxed::Payload>>()
            .push_on_response(
                svc::layers()
                    .push(metrics.stack.layer(stack_labels("balance.endpoint")))
                    .box_http_request(),
            )
            .push_spawn_ready()
            .check_make_service::<HttpEndpoint, http::Request<_>>()
            .push(discover)
            .check_service::<HttpConcrete>()
            .push_on_response(
                svc::layers()
                    .push(http::balance::layer(EWMA_DEFAULT_RTT, EWMA_DECAY))
                    // If the balancer has been empty/unavailable for 10s, eagerly fail
                    // requests.
                    .push_failfast(dispatch_timeout)
                    .push(metrics.stack.layer(stack_labels("concrete"))),
            )
            .into_new_service()
            .instrument(|c: &HttpConcrete| info_span!("concrete", dst = %c.dst))
            .check_new_service::<HttpConcrete, http::Request<_>>();

        // For each logical target, performs service profile resolution and
        // builds concrete services, over which requests are dispatched
        // (according to a split).
        //
        // Each service is cached, holding the profile and endpoint resolutions
        // and the load balancer with all of its endpoint connections.
        //
        // When no new requests have been dispatched for `cache_max_idle_age`,
        // the cached service is dropped. In-flight streams will continue to be
        // processed.
        let logical = concrete
            // Uses the split-provided target `Addr` to build a concrete target.
            .push_map_target(HttpConcrete::from)
            .push(profiles::split::layer())
            // Drives concrete stacks to readiness and makes the split
            // cloneable, as required by the retry middleware.
            .push_on_response(svc::layers().push_spawn_buffer(buffer_capacity))
            .push(profiles::http::route_request::layer(
                svc::proxies()
                    .push(metrics.http_route_actual.into_layer::<classify::Response>())
                    // Sets an optional retry policy.
                    .push(retry::layer(metrics.http_route_retry))
                    // Sets an optional request timeout.
                    .push(http::MakeTimeoutLayer::default())
                    // Records per-route metrics.
                    .push(metrics.http_route.into_layer::<classify::Response>())
                    // Sets the per-route response classifier as a request
                    // extension.
                    .push(classify::Layer::new())
                    .push_map_target(endpoint::route)
                    .into_inner(),
            ))
            .push_map_target(endpoint::Profile::from)
            // Discovers the service profile from the control plane and passes
            // it to inner stack to build the router and traffic split.
            .push(profiles::discover::layer(profiles_client))
            .into_new_service()
            .cache(
                svc::layers().push_on_response(
                    svc::layers()
                        .push_failfast(dispatch_timeout)
                        .push_spawn_buffer_with_idle_timeout(buffer_capacity, cache_max_idle_age)
                        .push(metrics.stack.layer(stack_labels("profile"))),
                ),
            )
            .spawn_buffer(buffer_capacity)
            .check_make_service::<HttpLogical, http::Request<_>>();

        // Caches clients that bypass discovery/balancing.
        let forward = svc::stack(endpoint)
            .check_make_service::<HttpEndpoint, http::Request<http::boxed::Payload>>()
            .into_new_service()
            .cache(
                svc::layers().push_on_response(
                    svc::layers()
                        .push_failfast(dispatch_timeout)
                        .push_spawn_buffer_with_idle_timeout(buffer_capacity, cache_max_idle_age)
                        .box_http_request()
                        .push(metrics.stack.layer(stack_labels("forward.endpoint"))),
                ),
            )
            .spawn_buffer(buffer_capacity)
            .instrument(|t: &HttpEndpoint| info_span!("forward", peer.addr = %t.addr, peer.id = ?t.identity))
            .check_make_service::<HttpEndpoint, http::Request<_>>();

        // Attempts to route route request to a logical services that uses
        // control plane for discovery. If the discovery is rejected, the
        // `forward` stack is used instead, bypassing load balancing, etc.
        logical
            .push_on_response(svc::layers().box_http_response())
            .push_make_ready()
            .push_fallback_with_predicate(
                forward
                    .push_map_target(HttpEndpoint::from)
                    .push_on_response(svc::layers().box_http_response().box_http_request())
                    .into_inner(),
                is_discovery_rejected,
            )
            .push(http::header_from_target::layer(CANONICAL_DST_HEADER))
            // Strips headers that may be set by this proxy.
            .push_on_response(http::strip_header::request::layer(DST_OVERRIDE_HEADER))
            .check_make_service_clone::<HttpLogical, http::Request<B>>()
            .instrument(|logical: &HttpLogical| info_span!("logical", dst = %logical.dst))
            .into_inner()
    }

    /// Constructs a TCP load balancer.
    pub fn build_tcp_balance<C, E, I>(
        &self,
        tcp_connect: &C,
        resolve: E,
        prevent_loop: PreventLoop,
        metrics: &ProxyMetrics,
    ) -> impl tower::Service<
        SocketAddr,
        Error = Error,
        Future = impl Unpin + Send + 'static,
        Response = impl tower::Service<
            I,
            Response = (),
            Future = impl Unpin + Send + 'static,
            Error = Error,
        > + Unpin
                       + Clone
                       + Send
                       + 'static,
    > + Unpin
           + Clone
           + Send
           + Sync
           + 'static
    where
        C: tower::Service<TcpEndpoint, Error = Error> + Unpin + Clone + Send + Sync + 'static,
        C::Response: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
        C::Future: Unpin + Send,
        E: Resolve<Addr, Endpoint = proxy::api_resolve::Metadata> + Unpin + Clone + Send + 'static,
        E::Future: Unpin + Send,
        E::Resolution: Unpin + Send,
        I: tokio::io::AsyncRead + tokio::io::AsyncWrite + std::fmt::Debug + Unpin + Send + 'static,
    {
        let ProxyConfig {
            dispatch_timeout,
            cache_max_idle_age,
            buffer_capacity,
            ..
        } = self.proxy;
        svc::stack(tcp_connect.clone())
            .push_make_thunk()
            .instrument(|t: &TcpEndpoint| info_span!("endpoint", peer.addr = %t.addr, peer.id = ?t.identity))
            .push(admit::AdmitLayer::new(prevent_loop))
            .check_make_service::<TcpEndpoint, ()>()
            .push(discover::resolve(map_endpoint::Resolve::new(
                endpoint::FromMetadata,
                resolve,
            )))
            .push(discover::buffer(1_000, cache_max_idle_age))
            .push_map_target(Addr::from)
            .push_on_response(tcp::balance::layer(EWMA_DEFAULT_RTT, EWMA_DECAY))
            .push_fallback_with_predicate(
                svc::stack(tcp_connect.clone())
                    .push_make_thunk()
                    .push(admit::AdmitLayer::new(prevent_loop))
                    .push_map_target(TcpEndpoint::from)
                    .instrument(|_: &SocketAddr| info_span!("forward")),
                is_discovery_rejected,
            )
            .into_new_service()
            .check_new_service::<SocketAddr, ()>()
            .cache(
                svc::layers().push_on_response(
                    svc::layers()
                        .push_failfast(dispatch_timeout)
                        .push_spawn_buffer_with_idle_timeout(buffer_capacity, cache_max_idle_age)
                        .push(metrics.stack.layer(stack_labels("tcp"))),
                ),
            )
            .spawn_buffer(buffer_capacity)
            .check_make_service::<SocketAddr, ()>()
            .push(svc::layer::mk(tcp::Forward::new))
            .instrument(|a: &SocketAddr| info_span!("tcp", dst = %a))
    }

    pub async fn build_server<E, R, C, H, S>(
        self,
        listen_addr: std::net::SocketAddr,
        listen: impl Stream<Item = std::io::Result<listen::Connection>> + Send + 'static,
        refine: R,
        resolve: E,
        tcp_connect: C,
        http_router: H,
        metrics: ProxyMetrics,
        span_sink: Option<mpsc::Sender<oc::Span>>,
        drain: drain::Watch,
    ) -> Result<(), Error>
    where
        E: Resolve<Addr, Endpoint = proxy::api_resolve::Metadata> + Unpin + Clone + Send + 'static,
        E::Future: Unpin + Send,
        E::Resolution: Unpin + Send,
        R: tower::Service<dns::Name, Error = Error, Response = dns::Name>
            + Unpin
            + Clone
            + Send
            + 'static,
        R::Future: Unpin + Send,
        C: tower::Service<TcpEndpoint, Error = Error> + Unpin + Clone + Send + Sync + 'static,
        C::Response: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
        C::Future: Unpin + Send,
        H: tower::Service<HttpLogical, Error = Error, Response = S>
            + Unpin
            + Send
            + Clone
            + 'static,
        H::Future: Unpin + Send,
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
            disable_protocol_detection_for_ports: ref skip_detect,
            dispatch_timeout,
            max_in_flight_requests,
            detect_protocol_timeout,
            ..
        } = self.proxy;
        let canonicalize_timeout = self.canonicalize_timeout;
        let prevent_loop = PreventLoop::from(listen_addr.port());

        // Load balances TCP streams that cannot be decoded as HTTP.
        let tcp_balance =
            svc::stack(self.build_tcp_balance(&tcp_connect, resolve, prevent_loop, &metrics))
                .push_map_target(|a: listen::Addrs| a.target_addr())
                .into_inner();

        let http_admit_request = svc::layers()
            // Limits the number of in-flight requests.
            .push_concurrency_limit(max_in_flight_requests)
            // Eagerly fail requests when the proxy is out of capacity for a
            // dispatch_timeout.
            .push_failfast(dispatch_timeout)
            .push(metrics.http_errors.clone())
            // Synthesizes responses for proxy errors.
            .push(errors::layer())
            // Initiates OpenCensus tracing.
            .push(TraceContextLayer::new(span_sink.clone().map(|span_sink| {
                SpanConverter::server(span_sink, trace_labels())
            })));

        let http_server = svc::stack(http_router)
            // Resolve the application-emitted destination via DNS to determine
            // its canonical FQDN to use for routing.
            .push(http::canonicalize::Layer::new(refine, canonicalize_timeout))
            .check_make_service::<HttpLogical, http::Request<_>>()
            .push_make_ready()
            .push_timeout(dispatch_timeout)
            .push(router::Layer::new(LogicalPerRequest::from))
            .check_new_service::<listen::Addrs, http::Request<_>>()
            // Used by tap.
            .push_http_insert_target()
            .push_on_response(
                svc::layers()
                    .push(http_admit_request)
                    .push(metrics.stack.layer(stack_labels("source")))
                    .box_http_request()
                    .box_http_response(),
            )
            .push(svc::layer::mk(http::normalize_uri::MakeNormalizeUri::new))
            .instrument(
                |addrs: &listen::Addrs| info_span!("source", target.addr = %addrs.target_addr()),
            )
            .check_new_service::<listen::Addrs, http::Request<_>>()
            .into_inner()
            .into_make_service();

        let http = http::DetectHttp::new(
            h2_settings,
            detect_protocol_timeout,
            http_server,
            tcp_balance,
            drain.clone(),
        );

        let tcp_forward = svc::stack(tcp_connect)
            .push_make_thunk()
            .push(svc::layer::mk(tcp::Forward::new))
            .push(admit::AdmitLayer::new(prevent_loop))
            .push_map_target(TcpEndpoint::from);

        let accept = svc::stack(svc::stack::MakeSwitch::new(
            skip_detect.clone(),
            http,
            tcp_forward,
        ))
        .push(metrics.transport.layer_accept(TransportLabels));

        info!(addr = %listen_addr, "Serving");
        serve::serve(listen, accept, drain.signal()).await
    }
}

fn stack_labels(name: &'static str) -> metric_labels::StackLabels {
    metric_labels::StackLabels::outbound(name)
}

#[derive(Copy, Clone, Debug)]
struct TransportLabels;

impl transport::metrics::TransportLabels<HttpEndpoint> for TransportLabels {
    type Labels = transport::labels::Key;

    fn transport_labels(&self, endpoint: &HttpEndpoint) -> Self::Labels {
        transport::labels::Key::Connect(endpoint.clone().into())
    }
}

impl transport::metrics::TransportLabels<TcpEndpoint> for TransportLabels {
    type Labels = transport::labels::Key;

    fn transport_labels(&self, endpoint: &TcpEndpoint) -> Self::Labels {
        transport::labels::Key::Connect(endpoint.clone().into())
    }
}

impl transport::metrics::TransportLabels<listen::Addrs> for TransportLabels {
    type Labels = transport::labels::Key;

    fn transport_labels(&self, _: &listen::Addrs) -> Self::Labels {
        const NO_TLS: tls::Conditional<()> = Conditional::None(tls::ReasonForNoPeerName::Loopback);
        transport::labels::Key::Accept(transport::labels::Direction::Out, NO_TLS.into())
    }
}

pub fn trace_labels() -> HashMap<String, String> {
    let mut l = HashMap::new();
    l.insert("direction".to_string(), "outbound".to_string());
    l
}

fn is_discovery_rejected(err: &Error) -> bool {
    fn is_rejected(err: &(dyn std::error::Error + 'static)) -> bool {
        err.is::<DiscoveryRejected>()
            || err.is::<profiles::InvalidProfileAddr>()
            || err.source().map(is_rejected).unwrap_or(false)
    }

    let rejected = is_rejected(&**err);
    tracing::debug!(rejected, %err);
    rejected
}
