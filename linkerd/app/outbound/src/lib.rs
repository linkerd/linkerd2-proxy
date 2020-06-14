//! Configures and runs the outbound proxy.
//!
//! The outound proxy is responsible for routing traffic from the local
//! application to external network endpoints.

// #![deny(warnings, rust_2018_idioms)]

pub use self::endpoint::{
    Concrete, HttpEndpoint, Logical, LogicalPerRequest, Profile, ProfilePerTarget, Target,
    TcpEndpoint,
};
use ::http::header::HOST;
use futures::future;
use linkerd2_app_core::{
    admit, classify,
    config::{ProxyConfig, ServerConfig},
    dns, drain, dst, errors, metric_labels,
    opencensus::proto::trace::v1 as oc,
    profiles,
    proxy::{
        self, core::resolve::Resolve, detect::DetectProtocolLayer, discover, http, identity,
        resolve::map_endpoint, server::ProtocolDetect, tap, tcp, Server,
    },
    reconnect, retry, router, serve,
    spans::SpanConverter,
    svc::{self, NewService},
    transport::{self, tls},
    Conditional, DiscoveryRejected, Error, ProxyMetrics, StackMetrics, TraceContextLayer,
    CANONICAL_DST_HEADER, DST_OVERRIDE_HEADER, L5D_CLIENT_ID, L5D_REMOTE_IP, L5D_REQUIRE_ID,
    L5D_SERVER_ID,
};
use std::collections::HashMap;
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::info_span;

// #[allow(dead_code)] // TODO #2597
// mod add_remote_ip_on_rsp;
// #[allow(dead_code)] // TODO #2597
// mod add_server_id_on_rsp;
pub mod endpoint;
mod orig_proto_upgrade;
mod prevent_loop;
mod require_identity_on_endpoint;

use self::orig_proto_upgrade::OrigProtoUpgradeLayer;
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
        Target<HttpEndpoint>,
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
            .instrument(|name: &dns::Name| info_span!("refine", %name))
            // Obtains the service, advances the state of the resolution
            .push(svc::make_response::Layer)
            // Ensures that the cache isn't locked when polling readiness.
            .push_oneshot()
            .into_inner()
    }

    pub fn build_http_router<B, C, R, P>(
        &self,
        prevent_loop: impl Into<PreventLoop>,
        tcp_connect: C,
        resolve: R,
        profiles_client: P,
        tap_layer: tap::Layer,
        metrics: ProxyMetrics,
        span_sink: Option<mpsc::Sender<oc::Span>>,
    ) -> impl tower::Service<
        Target<HttpEndpoint>,
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
        B: http::HttpBody + std::fmt::Debug + Default + Send + 'static,
        B::Data: Send + 'static,
        B::Error: Into<Error>,
        C: tower::Service<Target<HttpEndpoint>, Error = Error>
            + Unpin
            + Clone
            + Send
            + Sync
            + 'static,
        C::Response: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
        C::Future: Unpin + Send,
        R: Resolve<Concrete<http::Settings>, Endpoint = proxy::api_resolve::Metadata>
            + Unpin
            + Clone
            + Send
            + 'static,
        R::Future: Unpin + Send,
        R::Resolution: Unpin + Send,
        P: profiles::GetRoutes<Profile> + Unpin + Clone + Send + 'static,
        P::Future: Unpin + Send,
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

        let http_endpoint = {
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
                .push_on_response(
                    svc::layers()
                        .push(http::strip_header::response::layer(L5D_REMOTE_IP))
                        .push(http::strip_header::response::layer(L5D_SERVER_ID))
                        .push(http::strip_header::request::layer(L5D_REQUIRE_ID)),
                )
                .push(MakeRequireIdentityLayer::new());

            svc::stack(tcp_connect.clone())
                // Initiates an HTTP client on the underlying transport. Prior-knowledge HTTP/2
                // is typically used (i.e. when communicating with other proxies); though
                // HTTP/1.x fallback is supported as needed.
                .push(http::MakeClientLayer::new(connect.h2_settings))
                // Re-establishes a connection when the client fails.
                .push(reconnect::layer({
                    let backoff = connect.backoff.clone();
                    move |_| Ok(backoff.stream())
                }))
                .push(admit::AdmitLayer::new(prevent_loop))
                .push(observability.clone())
                .push(identity_headers.clone())
                .push(http::override_authority::Layer::new(vec![HOST.as_str(), CANONICAL_DST_HEADER]))
                // Ensures that the request's URI is in the proper form.
                .push(http::normalize_uri::layer())
                // Upgrades HTTP/1 requests to be transported over HTTP/2 connections.
                //
                // This sets headers so that the inbound proxy can downgrade the request
                // properly.
                .push(OrigProtoUpgradeLayer::new())
                .check_service::<Target<HttpEndpoint>>()
                .instrument(|endpoint: &Target<HttpEndpoint>| {
                    info_span!("endpoint", peer.addr = %endpoint.inner.addr)
                })
        };

        // Resolves each target via the control plane on a background task, buffering results.
        //
        // This buffer controls how many discovery updates may be pending/unconsumed by the
        // balancer before backpressure is applied on the resolution stream. If the buffer is
        // full for `cache_max_idle_age`, then the resolution task fails.
        let discover = {
            const BUFFER_CAPACITY: usize = 1_000;
            let resolve = map_endpoint::Resolve::new(endpoint::FromMetadata, resolve.clone());
            discover::Layer::new(BUFFER_CAPACITY, cache_max_idle_age, resolve)
        };

        // Builds a balancer for each concrete destination.
        let http_balancer = http_endpoint
            .clone()
            .check_make_service::<Target<HttpEndpoint>, http::Request<http::boxed::Payload>>()
            .push_on_response(
                svc::layers()
                    .push(metrics.stack.layer(stack_labels("balance.endpoint")))
                    .box_http_request(),
            )
            .push_spawn_ready()
            .check_service::<Target<HttpEndpoint>>()
            .push(discover)
            .push_on_response(http::balance::layer(EWMA_DEFAULT_RTT, EWMA_DECAY))
            .into_new_service()
            .cache(
                svc::layers().push_on_response(
                    svc::layers()
                        // If the balancer has been empty/unavailable for 10s, eagerly fail
                        // requests.
                        .push_failfast(dispatch_timeout)
                        // Shares the balancer, ensuring discovery errors are propagated.
                        .push_spawn_buffer_with_idle_timeout(buffer_capacity, cache_max_idle_age)
                        .push(metrics.stack.layer(stack_labels("balance"))),
                ),
            )
            .instrument(|c: &Concrete<http::Settings>| info_span!("balance", addr = %c.addr))
            // Ensure that buffers don't hold the cache's lock in poll_ready.
            .push_oneshot();

        // Caches clients that bypass discovery/balancing.
        //
        // This is effectively the same as the endpoint stack; but the client layer captures the
        // requst body type (via PhantomData), so the stack cannot be shared directly.
        let http_forward_cache = http_endpoint
            .check_make_service::<Target<HttpEndpoint>, http::Request<http::boxed::Payload>>()
            .into_new_service()
            .cache(
                svc::layers()
                    .push_on_response(
                        svc::layers()
                            // If the endpoint has been unavailable for an extended time, eagerly
                            // fail requests.
                            .push_failfast(dispatch_timeout)
                            // Shares the balancer, ensuring discovery errors are propagated.
                            .push_spawn_buffer_with_idle_timeout(buffer_capacity, cache_max_idle_age)
                            .box_http_request()
                            .push(metrics.stack.layer(stack_labels("forward.endpoint"))),
                    ),
            )
            .instrument(|endpoint: &Target<HttpEndpoint>| {
                info_span!("forward", peer.addr = %endpoint.addr, peer.id = ?endpoint.inner.identity)
            })
            .push_map_target(|t: Concrete<HttpEndpoint>| Target {
                addr: t.addr.into(),
                inner: t.inner.inner,
            })
            .check_service::<Concrete<HttpEndpoint>>()
            // Ensure that buffers don't hold the cache's lock in poll_ready.
            .push_oneshot();

        // If the balancer fails to be created, i.e., because it is unresolvable, fall back to
        // using a router that dispatches request to the application-selected original destination.
        let http_concrete = http_balancer
            .push_map_target(|c: Concrete<HttpEndpoint>| c.map(|l| l.map(|e| e.settings)))
            .check_service::<Concrete<HttpEndpoint>>()
            .push_on_response(svc::layers().box_http_response())
            .push_make_ready()
            .push_fallback_with_predicate(
                http_forward_cache
                    .push_on_response(svc::layers().box_http_response())
                    .into_inner(),
                is_discovery_rejected,
            )
            .check_service::<Concrete<HttpEndpoint>>();

        let http_profile_route_proxy = svc::proxies()
            .check_new_clone_service::<dst::Route>()
            .push(metrics.http_route_actual.into_layer::<classify::Response>())
            // Sets an optional retry policy.
            .push(retry::layer(metrics.http_route_retry))
            .check_new_clone_service::<dst::Route>()
            // Sets an optional request timeout.
            .push(http::MakeTimeoutLayer::default())
            .check_new_clone_service::<dst::Route>()
            // Records per-route metrics.
            .push(metrics.http_route.into_layer::<classify::Response>())
            .check_new_clone_service::<dst::Route>()
            // Sets the per-route response classifier as a request
            // extension.
            .push(classify::Layer::new())
            .check_new_clone_service::<dst::Route>();

        // Routes `Logical` targets to a cached `Profile` stack, i.e. so that profile
        // resolutions are shared even as the type of request may vary.
        let http_logical_profile_cache = http_concrete
            .clone()
            .push_on_response(svc::layers().box_http_request())
            .check_service::<Concrete<HttpEndpoint>>()
            // Provides route configuration. The profile service operates
            // over `Concret` services. When overrides are in play, the
            // Concrete destination may be overridden.
            .push(profiles::Layer::with_overrides(
                profiles_client,
                http_profile_route_proxy.into_inner(),
            ))
            .check_make_service::<Profile, Concrete<HttpEndpoint>>()
            // Use the `Logical` target as a `Concrete` target. It may be
            // overridden by the profile layer.
            .push_on_response(
                svc::layers().push_map_target(|inner: Logical<HttpEndpoint>| Concrete {
                    addr: inner.addr.clone(),
                    inner,
                }),
            )
            .into_new_service()
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
            .instrument(|_: &Profile| info_span!("profile"))
            // Ensures that the cache isn't locked when polling readiness.
            .push_oneshot()
            .check_make_service::<Profile, Logical<HttpEndpoint>>()
            .push(router::Layer::new(|()| ProfilePerTarget))
            .check_new_service_routes::<(), Logical<HttpEndpoint>>()
            .new_service(());

        // Routes requests to their logical target.
        svc::stack(http_logical_profile_cache)
            .check_service::<Logical<HttpEndpoint>>()
            .push_on_response(svc::layers().box_http_response())
            .push_make_ready()
            .push_fallback_with_predicate(
                http_concrete
                    .push_map_target(|inner: Logical<HttpEndpoint>| Concrete {
                        addr: inner.addr.clone(),
                        inner,
                    })
                    .push_on_response(svc::layers().box_http_response().box_http_request())
                    .check_service::<Logical<HttpEndpoint>>()
                    .into_inner(),
                is_discovery_rejected,
            )
            .check_service::<Logical<HttpEndpoint>>()
            .push(http::header_from_target::layer(CANONICAL_DST_HEADER))
            .push_on_response(
                // Strips headers that may be set by this proxy.
                svc::layers()
                    .push(http::strip_header::request::layer(L5D_CLIENT_ID))
                    .push(http::strip_header::request::layer(DST_OVERRIDE_HEADER)),
            )
            // Sets the canonical-dst header on all outbound requests.
            .check_service::<Logical<HttpEndpoint>>()
            .instrument(|logical: &Logical<_>| info_span!("logical", addr = %logical.addr))
            .into_inner()
    }

    pub fn build_server<C, R, H, S>(
        self,
        listen: transport::Listen<transport::DefaultOrigDstAddr>,
        prevent_loop: impl Into<PreventLoop>,
        refine: R,
        tcp_connect: C,
        http_router: H,
        metrics: ProxyMetrics,
        span_sink: Option<mpsc::Sender<oc::Span>>,
        drain: drain::Watch,
    ) -> Pin<Box<dyn Future<Output = Result<(), Error>> + Send + 'static>>
    where
        R: tower::Service<dns::Name, Error = Error, Response = dns::Name>
            + Unpin
            + Clone
            + Send
            + 'static,
        R::Future: Unpin + Send,
        C: tower::Service<TcpEndpoint, Error = Error> + Unpin + Clone + Send + Sync + 'static,
        C::Response: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
        C::Future: Unpin + Send,
        H: tower::Service<Target<HttpEndpoint>, Error = Error, Response = S>
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
        let Config {
            canonicalize_timeout,
            proxy:
                ProxyConfig {
                    server: ServerConfig { h2_settings, .. },
                    disable_protocol_detection_for_ports,
                    dispatch_timeout,
                    max_in_flight_requests,
                    detect_protocol_timeout,
                    ..
                },
        } = self;

        // The stack is served lazily since caching layers spawn tasks from
        // their constructor. This helps to ensure that tasks are spawned on the
        // same runtime as the proxy.
        // Forwards TCP streams that cannot be decoded as HTTP.
        let tcp_forward = svc::stack(tcp_connect)
            .push(admit::AdmitLayer::new(prevent_loop.into()))
            .push_map_target(|meta: tls::accept::Meta| TcpEndpoint::from(meta.addrs.target_addr()))
            .push(svc::layer::mk(tcp::Forward::new));

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
            })))
            // Tracks proxy handletime.
            .push(metrics.clone().http_handle_time.layer());

        let http_server = svc::stack(http_router)
            // Resolve the application-emitted destination via DNS to determine                                                                                                                      
            // its canonical FQDN to use for routing.                                                                                                                                                
            .push(http::canonicalize::Layer::new(refine, canonicalize_timeout))
            .check_make_service::<Logical<HttpEndpoint>, http::Request<_>>()
            .push_make_ready()
            .push_timeout(dispatch_timeout)
            .push(router::Layer::new(LogicalPerRequest::from))
            .check_new_service::<tls::accept::Meta>()
            // Used by tap.
            .push_http_insert_target()
            .push_on_response(
                 svc::layers()
                    .push(http_admit_request)
                    .push(metrics.stack.layer(stack_labels("source")))
                    .box_http_request()
            )
            .instrument(
                |src: &tls::accept::Meta| {
                    info_span!("source", target.addr = %src.addrs.target_addr())
                },
            )
            .check_new_service::<tls::accept::Meta>();

        let tcp_server = Server::new(
            TransportLabels,
            metrics.transport,
            tcp_forward.into_inner(),
            http_server.into_inner(),
            h2_settings,
            drain.clone(),
        );

        let no_tls: tls::Conditional<identity::Local> =
            Conditional::None(tls::ReasonForNoPeerName::Loopback.into());

        let tcp_detect = svc::stack(tcp_server)
            .push(DetectProtocolLayer::new(ProtocolDetect::new(
                disable_protocol_detection_for_ports.clone(),
            )))
            // The local application never establishes mTLS with the proxy, so don't try to
            // terminate TLS, just annotate with the connection with the reason.
            .push(tls::AcceptTls::layer(
                no_tls,
                disable_protocol_detection_for_ports,
            ))
            // Limits the amount of time that the TCP server spends waiting for protocol
            // detection. Ensures that connections that never emit data are dropped eventually.
            .push_timeout(detect_protocol_timeout);

        Box::pin(serve::serve(listen, tcp_detect.into_inner(), drain))
    }
}

fn stack_labels(name: &'static str) -> metric_labels::StackLabels {
    metric_labels::StackLabels::outbound(name)
}

#[derive(Copy, Clone, Debug)]
struct TransportLabels;

impl transport::metrics::TransportLabels<Target<HttpEndpoint>> for TransportLabels {
    type Labels = transport::labels::Key;

    fn transport_labels(&self, endpoint: &Target<HttpEndpoint>) -> Self::Labels {
        transport::labels::Key::connect("outbound", endpoint.inner.identity.as_ref())
    }
}

impl transport::metrics::TransportLabels<TcpEndpoint> for TransportLabels {
    type Labels = transport::labels::Key;

    fn transport_labels(&self, endpoint: &TcpEndpoint) -> Self::Labels {
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

#[derive(Clone, Debug)]
enum DiscoveryError {
    DiscoveryRejected,
    Inner(String),
}

impl std::fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiscoveryError::DiscoveryRejected => write!(f, "discovery rejected"),
            DiscoveryError::Inner(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for DiscoveryError {}

impl From<Error> for DiscoveryError {
    fn from(orig: Error) -> Self {
        if let Some(inner) = orig.downcast_ref::<DiscoveryError>() {
            return inner.clone();
        }

        if orig.is::<DiscoveryRejected>()
        //  || orig.is::<profiles::InvalidProfileAddr>()
        {
            return DiscoveryError::DiscoveryRejected;
        }

        DiscoveryError::Inner(orig.to_string())
    }
}

fn is_discovery_rejected(err: &Error) -> bool {
    tracing::trace!(?err, "is_discovery_rejected");

    if let Some(e) = err.downcast_ref::<svc::buffer::error::ServiceError>() {
        return is_discovery_rejected(e.inner());
    }
    if let Some(e) = err.downcast_ref::<svc::lock::error::ServiceError>() {
        return is_discovery_rejected(e.inner());
    }

    err.is::<DiscoveryRejected>() || err.is::<profiles::InvalidProfileAddr>()
}
