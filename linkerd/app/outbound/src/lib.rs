//! Configures and runs the outbound proxy.
//!
//! The outound proxy is responsible for routing traffic from the local
//! application to external network endpoints.

#![deny(warnings, rust_2018_idioms)]

use self::allow_discovery::AllowProfile;
pub use self::endpoint::{HttpConcrete, HttpEndpoint, HttpLogical, TcpEndpoint};
use futures::future;
use linkerd2_app_core::{
    classify,
    config::{ProxyConfig, ServerConfig},
    drain, errors, metric_labels,
    opencensus::proto::trace::v1 as oc,
    profiles,
    proxy::{
        self, core::resolve::Resolve, discover, http, identity, resolve::map_endpoint, tap, tcp,
    },
    reconnect, retry,
    spans::SpanConverter,
    svc::{self},
    transport::{self, listen, tls},
    Addr, Conditional, Error, IpMatch, ProxyMetrics, TraceContextLayer, CANONICAL_DST_HEADER,
    DST_OVERRIDE_HEADER, L5D_REQUIRE_ID,
};
use std::{collections::HashMap, net::SocketAddr, time::Duration};
use tokio::sync::mpsc;
use tracing::{debug_span, info_span};

mod allow_discovery;
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
    pub allow_discovery: IpMatch,
}

impl Config {
    pub fn build_tcp_connect(
        &self,
        prevent_loop: impl Into<PreventLoop>,
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
            .push_request_filter(prevent_loop.into())
            .into_inner()
    }

    /// Constructs a TCP load balancer.
    pub fn build_tcp_balance<C, E, I>(
        &self,
        connect: C,
        resolve: E,
    ) -> impl svc::NewService<
        endpoint::TcpLogical,
        Service = impl tower::Service<
            I,
            Response = (),
            Future = impl Unpin + Send + 'static,
            Error = impl Into<Error>,
        > + Unpin
                      + Send
                      + 'static,
    > + Clone
           + Unpin
           + Send
           + 'static
    where
        C: tower::Service<TcpEndpoint, Error = Error> + Unpin + Clone + Send + Sync + 'static,
        C::Response: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
        C::Future: Unpin + Send,
        E: Resolve<endpoint::TcpLogical, Endpoint = proxy::api_resolve::Metadata>
            + Unpin
            + Clone
            + Send
            + 'static,
        E::Future: Unpin + Send,
        E::Resolution: Unpin + Send,
        I: tokio::io::AsyncRead + tokio::io::AsyncWrite + std::fmt::Debug + Unpin + Send + 'static,
    {
        svc::stack(connect)
            .push_make_thunk()
            .instrument(|t: &TcpEndpoint| info_span!("endpoint", peer.addr = %t.addr, peer.id = ?t.identity))
            .check_make_service::<TcpEndpoint, ()>()
            .push(discover::resolve(map_endpoint::Resolve::new(
                endpoint::FromMetadata,
                resolve,
            )))
            .push(discover::buffer(1_000, self.proxy.cache_max_idle_age))
            .push_on_response(
                svc::layers()
                    .push(tcp::balance::layer(EWMA_DEFAULT_RTT, EWMA_DECAY))
                    .push(svc::layer::mk(tcp::Forward::new))
            )
            .into_new_service()
            .check_new_service::<endpoint::TcpLogical, I>()
    }

    pub fn build_http_endpoint<B, C>(
        &self,
        tcp_connect: C,
        tap_layer: tap::Layer,
        metrics: ProxyMetrics,
        span_sink: Option<mpsc::Sender<oc::Span>>,
    ) -> impl svc::NewService<
        HttpEndpoint,
        Service = impl tower::Service<
            http::Request<B>,
            Response = http::Response<http::boxed::Payload>,
            Error = Error,
            Future = impl Send,
        > + Send,
    > + Clone
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
                move |e: Error| {
                    if is_loop(&*e) {
                        Err(e)
                    } else {
                        Ok(backoff.stream())
                    }
                }
            }))
            .check_new::<HttpEndpoint>()
            .push(observability.clone())
            .push(identity_headers.clone())
            .push(http::override_authority::Layer::new(vec![
                ::http::header::HOST.as_str(),
                CANONICAL_DST_HEADER,
            ]))
            .push_on_response(svc::layers().box_http_response())
            .check_new::<HttpEndpoint>()
            .instrument(|e: &HttpEndpoint| info_span!("endpoint", peer.addr = %e.addr))
            .into_inner()
    }

    pub fn build_http_router<B, E, S, R>(
        &self,
        endpoint: E,
        resolve: R,
        metrics: ProxyMetrics,
    ) -> impl svc::NewService<
        HttpLogical,
        Service = impl tower::Service<
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
        E: svc::NewService<HttpEndpoint, Service = S> + Clone + Send + Sync + Unpin + 'static,
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
    {
        let ProxyConfig {
            buffer_capacity,
            cache_max_idle_age,
            dispatch_timeout,
            ..
        } = self.proxy.clone();

        svc::stack(endpoint)
            .check_new_service::<HttpEndpoint, http::Request<http::boxed::Payload>>()
            .push_on_response(
                svc::layers()
                    .push(svc::layer::mk(svc::SpawnReady::new))
                    .push(metrics.stack.layer(stack_labels("balance.endpoint")))
                    .box_http_request(),
            )
            .check_new_service::<HttpEndpoint, http::Request<_>>()
            .push(discover::resolve(map_endpoint::Resolve::new(
                endpoint::FromMetadata,
                resolve,
            )))
            .push(discover::buffer(1_000, cache_max_idle_age))
            .check_service::<HttpConcrete>()
            .push_on_response(
                svc::layers()
                    .push(http::balance::layer(EWMA_DEFAULT_RTT, EWMA_DECAY))
                    .push(svc::layer::mk(svc::SpawnReady::new))
                    // If the balancer has been empty/unavailable for 10s, eagerly fail
                    // requests.
                    .push_failfast(dispatch_timeout)
                    .push(metrics.stack.layer(stack_labels("concrete"))),
            )
            .into_new_service()
            .check_new_service::<HttpConcrete, http::Request<_>>()
            .instrument(|c: &HttpConcrete| match c.resolve.as_ref() {
                None => info_span!("concrete"),
                Some(addr) => info_span!("concrete", %addr),
            })
            .check_new_service::<HttpConcrete, http::Request<_>>()
            // The concrete address is only set when the profile could be
            // resolved. Endpoint resolution is skipped when there is no
            // concrete address.
            .push_map_target(HttpConcrete::from)
            .check_new_service::<(Option<Addr>, HttpLogical), http::Request<_>>()
            .push(profiles::split::layer())
            .check_new_service::<HttpLogical, http::Request<_>>()
            // Drives concrete stacks to readiness and makes the split
            // cloneable, as required by the retry middleware.
            .push_on_response(
                svc::layers()
                    .push_failfast(dispatch_timeout)
                    .push_spawn_buffer(buffer_capacity),
            )
            .check_new_service::<HttpLogical, http::Request<_>>()
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
            .check_new_service::<HttpLogical, http::Request<_>>()
            .push(http::header_from_target::layer(CANONICAL_DST_HEADER))
            .push_on_response(
                svc::layers()
                    // Strips headers that may be set by this proxy.
                    .push(http::strip_header::request::layer(DST_OVERRIDE_HEADER))
                    .push(svc::layers().box_http_response()),
            )
            .instrument(|l: &HttpLogical| info_span!("logical", dst = %l.addr()))
            .check_new_service::<HttpLogical, http::Request<_>>()
            .into_inner()
    }

    pub fn build_server<E, P, C, H, S, I>(
        self,
        profiles: P,
        endpoints: E,
        tcp_connect: C,
        http_router: H,
        metrics: ProxyMetrics,
        span_sink: Option<mpsc::Sender<oc::Span>>,
        drain: drain::Watch,
    ) -> impl svc::NewService<
        listen::Addrs,
        Service = impl tower::Service<
            I,
            Response = (),
            Error = impl Into<Error>,
            Future = impl Send + 'static,
        > + Send
                      + 'static,
    > + Send
           + 'static
    where
        I: tokio::io::AsyncRead + tokio::io::AsyncWrite + std::fmt::Debug + Unpin + Send + 'static,
        E: Resolve<endpoint::TcpLogical, Endpoint = proxy::api_resolve::Metadata>
            + Unpin
            + Clone
            + Send
            + 'static,
        E::Future: Unpin + Send,
        E::Resolution: Unpin + Send,
        C: tower::Service<TcpEndpoint, Error = Error> + Unpin + Clone + Send + Sync + 'static,
        C::Response: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
        C::Future: Unpin + Send,
        H: svc::NewService<HttpLogical, Service = S> + Unpin + Send + Clone + 'static,
        S: tower::Service<
                http::Request<http::boxed::Payload>,
                Response = http::Response<http::boxed::Payload>,
                Error = Error,
            > + Send
            + 'static,
        S::Future: Send,
        P: profiles::GetProfile<SocketAddr> + Unpin + Clone + Send + 'static,
        P::Future: Unpin + Send,
        P::Error: Send,
    {
        let ProxyConfig {
            server: ServerConfig { h2_settings, .. },
            disable_protocol_detection_for_ports: ref skip_detect,
            dispatch_timeout,
            max_in_flight_requests,
            detect_protocol_timeout,
            cache_max_idle_age,
            buffer_capacity,
            ..
        } = self.proxy;

        let http_server = svc::stack(http_router)
            .check_new_service::<endpoint::HttpLogical, http::Request<_>>()
            .push_on_response(
                svc::layers()
                    .box_http_request()
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
                    .push(metrics.stack.layer(stack_labels("source")))
                    .push_failfast(dispatch_timeout)
                    .push_spawn_buffer(buffer_capacity)
                    .box_http_response(),
            )
            .check_new_service::<endpoint::HttpLogical, http::Request<_>>()
            .push(svc::layer::mk(http::normalize_uri::MakeNormalizeUri::new))
            .instrument(|l: &endpoint::HttpLogical| info_span!("http", v = %l.version))
            .push_map_target(endpoint::HttpLogical::from)
            .check_new_service::<(http::Version, endpoint::TcpLogical), http::Request<_>>()
            .into_inner();

        // Load balances TCP streams that cannot be decoded as HTTP.
        let tcp_balance = svc::stack(self.build_tcp_balance(tcp_connect.clone(), endpoints))
            .push_on_response(
                svc::layers()
                    .push_failfast(dispatch_timeout)
                    .push_spawn_buffer_with_idle_timeout(buffer_capacity, cache_max_idle_age),
            )
            .instrument(|_: &_| info_span!("tcp"))
            .check_new_service::<endpoint::TcpLogical, transport::io::PrefixedIo<transport::metrics::SensorIo<I>>>()
            .into_inner();

        let http = svc::stack(http::DetectHttp::new(
            h2_settings,
            http_server,
            tcp_balance,
            drain.clone(),
        ))
        .check_new_service::<
            endpoint::TcpLogical,
            transport::io::PrefixedIo<transport::metrics::SensorIo<I>>,
        >()
        .push_on_response(transport::Prefix::layer(
            http::Version::DETECT_BUFFER_CAPACITY,
            detect_protocol_timeout,
        ))
        .check_new_service::<endpoint::TcpLogical, transport::metrics::SensorIo<I>>()
        .into_inner();

        let tcp = svc::stack(tcp_connect)
            .push_make_thunk()
            .push_on_response(svc::layer::mk(tcp::Forward::new))
            .instrument(|_: &TcpEndpoint| debug_span!("tcp.forward"))
            .check_new_service::<TcpEndpoint, transport::metrics::SensorIo<I>>()
            .push_map_target(TcpEndpoint::from)
            .into_inner();

        svc::stack(svc::stack::MakeSwitch::new(skip_detect.clone(), http, tcp))
            .check_new_service::<endpoint::TcpLogical, transport::metrics::SensorIo<I>>()
            .push_map_target(endpoint::TcpLogical::from)
            .push(profiles::discover::layer(
                profiles,
                AllowProfile(self.allow_discovery),
            ))
            .check_new_service::<endpoint::TcpAccept, transport::metrics::SensorIo<I>>()
            .cache(
                svc::layers().push_on_response(
                    svc::layers()
                        .push_failfast(dispatch_timeout)
                        .push_spawn_buffer_with_idle_timeout(buffer_capacity, cache_max_idle_age),
                ),
            )
            .check_new_service::<endpoint::TcpAccept, transport::metrics::SensorIo<I>>()
            .push_map_target(endpoint::TcpAccept::from)
            .push(metrics.transport.layer_accept(TransportLabels))
            .check_new_service::<listen::Addrs, I>()
            .into_inner()
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
        const NO_TLS: tls::Conditional<identity::Name> =
            Conditional::None(tls::ReasonForNoPeerName::Loopback);
        transport::labels::Key::accept(transport::labels::Direction::Out, NO_TLS.into())
    }
}

pub fn trace_labels() -> HashMap<String, String> {
    let mut l = HashMap::new();
    l.insert("direction".to_string(), "outbound".to_string());
    l
}

fn is_loop(err: &(dyn std::error::Error + 'static)) -> bool {
    err.is::<prevent_loop::LoopPrevented>() || err.source().map(is_loop).unwrap_or(false)
}
