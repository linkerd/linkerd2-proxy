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
    // classify,
    config::{ProxyConfig, ServerConfig},
    dns,
    drain,
    // dst,
    // errors,
    metric_labels,
    opencensus::proto::trace::v1 as oc,
    profiles,
    proxy::{
        self, core::resolve::Resolve, detect::DetectProtocolLayer, discover, http, identity,
        resolve::map_endpoint, server::ProtocolDetect, tap, tcp, Server,
    },
    reconnect,
    // retry,
    router,
    serve,
    spans::SpanConverter,
    svc::{self, NewService},
    transport::{self, tls},
    Conditional,
    DiscoveryRejected,
    Error,
    ProxyMetrics,
    TraceContextLayer,
    CANONICAL_DST_HEADER,
    DST_OVERRIDE_HEADER,
    L5D_CLIENT_ID,
    L5D_REMOTE_IP,
    L5D_REQUIRE_ID,
    L5D_SERVER_ID,
};
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::info_span;

// #[allow(dead_code)] // TODO #2597
// mod add_remote_ip_on_rsp;
// #[allow(dead_code)] // TODO #2597
// mod add_server_id_on_rsp;
mod endpoint;
// mod orig_proto_upgrade;
// mod require_identity_on_endpoint;

// use self::orig_proto_upgrade::OrigProtoUpgradeLayer;
// use self::require_identity_on_endpoint::MakeRequireIdentityLayer;

const EWMA_DEFAULT_RTT: Duration = Duration::from_millis(30);
const EWMA_DECAY: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct Config {
    pub proxy: ProxyConfig,
    pub canonicalize_timeout: Duration,
}

pub struct Outbound {
    pub listen_addr: SocketAddr,
    pub serve: Pin<Box<dyn Future<Output = Result<(), Error>> + 'static>>,
}

impl Config {
    pub fn build<R, P>(
        self,
        local_identity: tls::Conditional<identity::Local>,
        // resolve: R,
        dns_resolver: dns::Resolver,
        // profiles_client: P,
        // tap_layer: tap::Layer,
        metrics: ProxyMetrics,
        span_sink: Option<mpsc::Sender<oc::Span>>,
        drain: drain::Watch,
    ) -> Result<Outbound, Error>
where
        // R: Resolve<Concrete<http::Settings>, Endpoint = proxy::api_resolve::Metadata>
        //     + Clone
        //     + Send
        //     + Sync
        //     + 'static,
        // R::Future: Send,
        // R::Resolution: Send,
        // P: profiles::GetRoutes<Profile> + Clone + Send + 'static,
        // P::Future: Send,
    {
        use proxy::core::listen::{Bind, Listen};
        let Config {
            canonicalize_timeout,
            proxy:
                ProxyConfig {
                    server: ServerConfig { bind, h2_settings },
                    connect,
                    buffer_capacity,
                    cache_max_idle_age,
                    disable_protocol_detection_for_ports,
                    dispatch_timeout,
                    max_in_flight_requests,
                    detect_protocol_timeout,
                },
        } = self;

        let listen = bind.bind().map_err(Error::from)?;
        let listen_addr = listen.listen_addr();

        // The stack is served lazily since caching layers spawn tasks from
        // their constructor. This helps to ensure that tasks are spawned on the
        // same runtime as the proxy.
        let serve = Box::pin(async move {
            // Establishes connections to remote peers (for both TCP
            // forwarding and HTTP proxying).
            let tcp_connect = svc::connect(connect.keepalive)
                // Initiates mTLS if the target is configured with identity.
                .push(tls::client::ConnectLayer::new(local_identity))
                // Limits the time we wait for a connection to be established.
                .push_timeout(connect.timeout)
                // .push(metrics.transport.layer_connect(TransportLabels))
                ;

            // Forwards TCP streams that cannot be decoded as HTTP.
            let tcp_forward = tcp_connect
                .clone()
                .push_map_target(|meta: tls::accept::Meta| {
                    TcpEndpoint::from(meta.addrs.target_addr())
                })
                .push(svc::layer::mk(tcp::Forward::new));

            // // Registers the stack with Tap, Metrics, and OpenCensus tracing
            // // export.
            // let http_endpoint = {
            //     let observability = svc::layers()
            //         .push(tap_layer.clone())
            //         .push(metrics.http_endpoint.into_layer::<classify::Response>())
            //         .push_on_response(TraceContextLayer::new(
            //             span_sink
            //                 .clone()
            //                 .map(|sink| SpanConverter::client(sink, trace_labels())),
            //         ));

            //     // Checks the headers to validate that a client-specified required
            //     // identity matches the configured identity.
            //     let identity_headers = svc::layers()
            //         .push_on_response(
            //             svc::layers()
            //                 .push(http::strip_header::response::layer(L5D_REMOTE_IP))
            //                 .push(http::strip_header::response::layer(L5D_SERVER_ID))
            //                 .push(http::strip_header::request::layer(L5D_REQUIRE_ID)),
            //         )
            //         .push(MakeRequireIdentityLayer::new());

            //     tcp_connect
            //         .clone()
            //         // Initiates an HTTP client on the underlying transport. Prior-knowledge HTTP/2
            //         // is typically used (i.e. when communicating with other proxies); though
            //         // HTTP/1.x fallback is supported as needed.
            //         .push(http::MakeClientLayer::new(connect.h2_settings))
            //         // Re-establishes a connection when the client fails.
            //         .push(reconnect::layer({
            //             let backoff = connect.backoff.clone();
            //             move |_| Ok(backoff.stream())
            //         }))
            //         .push(observability.clone())
            //         .push(identity_headers.clone())
            //         .push(http::override_authority::Layer::new(vec![HOST.as_str(), CANONICAL_DST_HEADER]))
            //         // Ensures that the request's URI is in the proper form.
            //         .push(http::normalize_uri::layer())
            //         // Upgrades HTTP/1 requests to be transported over HTTP/2 connections.
            //         //
            //         // This sets headers so that the inbound proxy can downgrade the request
            //         // properly.
            //         .push(OrigProtoUpgradeLayer::new())
            //         .check_service::<Target<HttpEndpoint>>()
            //         .instrument(|endpoint: &Target<HttpEndpoint>| {
            //             info_span!("endpoint", peer.addr = %endpoint.inner.addr)
            //         })
            // };

            // // Resolves each target via the control plane on a background task, buffering results.
            // //
            // // This buffer controls how many discovery updates may be pending/unconsumed by the
            // // balancer before backpressure is applied on the resolution stream. If the buffer is
            // // full for `cache_max_idle_age`, then the resolution task fails.
            // let discover = {
            //     const BUFFER_CAPACITY: usize = 1_000;
            //     let resolve = map_endpoint::Resolve::new(endpoint::FromMetadata, resolve.clone());
            //     discover::Layer::new(BUFFER_CAPACITY, cache_max_idle_age, resolve)
            // };

            // // Builds a balancer for each concrete destination.
            // let http_balancer = http_endpoint
            //     .clone()
            //     .check_make_service::<Target<HttpEndpoint>, http::Request<http::boxed::Payload>>()
            //     .push_on_response(
            //         svc::layers()
            //             .push(metrics.stack.layer(stack_labels("balance.endpoint")))
            //             .box_http_request(),
            //     )
            //     .push_spawn_ready()
            //     .check_service::<Target<HttpEndpoint>>()
            //     .push(discover)
            //     .push_on_response(http::balance::layer(EWMA_DEFAULT_RTT, EWMA_DECAY))
            //     .into_new_service()
            //     .cache(
            //         svc::layers().push_on_response(
            //             svc::layers()
            //                 // If the balancer has been ready & unused for `cache_max_idle_age`,
            //                 // fail the balancer.
            //                 .push_idle_timeout(cache_max_idle_age)
            //                 // If the balancer has been empty/unavailable for 10s, eagerly fail
            //                 // requests.
            //                 .push_failfast(dispatch_timeout)
            //                 // Shares the balancer, ensuring discovery errors are propagated.
            //                 .push_spawn_buffer(buffer_capacity)
            //                 .push(metrics.stack.layer(stack_labels("balance"))),
            //         ),
            //     )
            //     .instrument(|c: &Concrete<http::Settings>| info_span!("balance", addr = %c.addr))
            //     // Ensure that buffers don't hold the cache's lock in poll_ready.
            //     .push_oneshot();

            // // Caches clients that bypass discovery/balancing.
            // //
            // // This is effectively the same as the endpoint stack; but the client layer captures the
            // // requst body type (via PhantomData), so the stack cannot be shared directly.
            // let http_forward_cache = http_endpoint
            //     .check_make_service::<Target<HttpEndpoint>, http::Request<http::boxed::Payload>>()
            //     .into_new_service()
            //     .cache(
            //         svc::layers()
            //             .push_on_response(
            //                 svc::layers()
            //                     // If the endpoint has been ready & unused for `cache_max_idle_age`,
            //                     // fail it.
            //                     .push_idle_timeout(cache_max_idle_age)
            //                     // If the endpoint has been unavailable for an extend time, eagerly
            //                     // fail requests.
            //                     .push_failfast(dispatch_timeout)
            //                     // Shares the balancer, ensuring discovery errors are propagated.
            //                     .push_spawn_buffer(buffer_capacity)
            //                     .box_http_request()
            //                     .push(metrics.stack.layer(stack_labels("forward.endpoint"))),
            //             ),
            //     )
            //     .instrument(|endpoint: &Target<HttpEndpoint>| {
            //         info_span!("forward", peer.addr = %endpoint.addr, peer.id = ?endpoint.inner.identity)
            //     })
            //     .push_map_target(|t: Concrete<HttpEndpoint>| Target {
            //         addr: t.addr.into(),
            //         inner: t.inner.inner,
            //     })
            // };

            let http_server = |_| {
                tower::service_fn(move |_: http::Request<_>| async {
                    Ok(http::Response::new(http::Body::default()))
                })
            };

            let tcp_server = Server::new(
                TransportLabels,
                // metrics.transport,
                tcp_forward.into_inner(),
                http_server, /* .into_inner(), */
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

            serve::serve(listen, tcp_detect.into_inner(), drain).await
        });

        Ok(Outbound { listen_addr, serve })
    }
}

// fn stack_labels(name: &'static str) -> metric_labels::StackLabels {
//     metric_labels::StackLabels::outbound(name)
// }

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

    err.is::<DiscoveryRejected>()
    //  || err.is::<profiles::InvalidProfileAddr>()
}
