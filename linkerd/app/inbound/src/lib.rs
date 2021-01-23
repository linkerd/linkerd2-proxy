//! Configures and runs the inbound proxy.
//!
//! The inbound proxy is responsible for terminating traffic from other network
//! endpoints inbound to the local application.

#![deny(warnings, rust_2018_idioms)]

mod allow_discovery;
mod prevent_loop;
mod require_identity;
pub mod target;

pub use self::target::{
    HttpEndpoint, Logical, ProfileTarget, RequestTarget, Target, TcpAccept, TcpEndpoint,
};
use self::{
    allow_discovery::AllowProfile,
    prevent_loop::PreventLoop,
    require_identity::{RequireIdentityForDirect, RequireIdentityForPorts},
};
use linkerd_app_core::{
    classify,
    config::{ConnectConfig, ProxyConfig, ServerConfig},
    detect, drain, dst, errors, io, metrics,
    opencensus::proto::trace::v1 as oc,
    profiles,
    proxy::{
        http::{self, orig_proto, strip_header},
        identity::LocalCrtKey,
        tap, tcp,
    },
    reconnect,
    spans::SpanConverter,
    svc, tls,
    transport::{self, listen},
    transport_header::{DetectHeader, TransportHeader},
    Error, NameAddr, NameMatch, TraceContext, DST_OVERRIDE_HEADER,
};
use std::{collections::HashMap, fmt::Debug, net::SocketAddr, time::Duration};
use tokio::sync::mpsc;
use tracing::debug_span;

#[derive(Clone, Debug)]
pub struct Config {
    pub allow_discovery: NameMatch,
    pub proxy: ProxyConfig,
    pub require_identity_for_inbound_ports: RequireIdentityForPorts,
    pub disable_protocol_detection_for_ports: SkipByPort,
    pub profile_idle_timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct SkipByPort(std::sync::Arc<indexmap::IndexSet<u16>>);

#[derive(Default)]
struct NonOpaqueRefused(());

// === impl Config ===

pub fn tcp_connect<T: Into<u16>>(
    config: &ConnectConfig,
) -> impl svc::Service<
    T,
    Response = impl io::AsyncRead + io::AsyncWrite + Send,
    Error = Error,
    Future = impl Send,
> + Clone {
    // Establishes connections to remote peers (for both TCP
    // forwarding and HTTP proxying).
    svc::stack(transport::ConnectTcp::new(config.keepalive))
        .push_map_target(|t: T| ([127, 0, 0, 1], t.into()))
        // Limits the time we wait for a connection to be established.
        .push_timeout(config.timeout)
        .push(svc::stack::BoxFuture::layer())
        .into_inner()
}

#[allow(clippy::too_many_arguments)]
impl Config {
    pub fn build<I, C, L, LSvc, P>(
        self,
        listen_addr: SocketAddr,
        local_identity: Option<LocalCrtKey>,
        connect: C,
        http_loopback: L,
        profiles_client: P,
        tap: tap::Registry,
        metrics: metrics::Proxy,
        span_sink: Option<mpsc::Sender<oc::Span>>,
        drain: drain::Watch,
    ) -> impl svc::NewService<
        listen::Addrs,
        Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send>,
    > + Clone
    where
        I: io::AsyncRead + io::AsyncWrite + io::Peek + io::PeerAddr,
        I: Debug + Send + Sync + Unpin + 'static,
        C: svc::Service<TcpEndpoint> + Clone + Send + Sync + Unpin + 'static,
        C::Response: io::AsyncRead + io::AsyncWrite + Send + Unpin + 'static,
        C::Error: Into<Error>,
        C::Future: Send + Unpin,
        L: svc::NewService<Target, Service = LSvc> + Clone + Send + Sync + Unpin + 'static,
        LSvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>
            + Send
            + 'static,
        LSvc::Error: Into<Error>,
        LSvc::Future: Send,
        P: profiles::GetProfile<NameAddr> + Clone + Send + Sync + 'static,
        P::Error: Send,
        P::Future: Send,
    {
        let prevent_loop = PreventLoop::from(listen_addr.port());
        let detect_timeout = self.proxy.detect_protocol_timeout;
        let dispatch_timeout = self.proxy.dispatch_timeout;

        // Forwards TCP streams that cannot be decoded as HTTP.
        //
        // Looping is always prevented.
        let tcp_forward = svc::stack(connect.clone())
            .push_request_filter(prevent_loop)
            .push(metrics.transport.layer_connect())
            .push_make_thunk()
            .push_on_response(
                svc::layers()
                    .push(tcp::Forward::layer())
                    .push(drain::Retain::layer(drain.clone())),
            )
            .instrument(|_: &_| debug_span!("tcp"))
            .check_new::<TcpEndpoint>();

        // The direct stack handles connections that target the proxy's inbound
        // port (i.e. without an SO_ORIGINAL_DST setting). This port behaves
        // differently from the main proxy stack:
        //
        // 1. Protocol detection is always performed;
        // 2. TLS is required;
        // 3. A transport header is expected. It's not strictly required, as
        //    gateways may need to accept HTTP requests from older proxy versions.
        let direct = {
            // Direct traffic may target an HTTP gateway.
            let http = svc::stack(http_loopback)
                .push_on_response(
                    svc::layers()
                        .push(svc::FailFast::layer("Gateway", dispatch_timeout))
                        .push_spawn_buffer(self.proxy.buffer_capacity),
                )
                .check_new_service::<Target, http::Request<http::BoxBody>>()
                // Removes the override header after it has been used to
                // determine a reuquest target.
                .push_on_response(strip_header::request::layer(DST_OVERRIDE_HEADER))
                // Routes each request to a target, obtains a service for that
                // target, and dispatches the request.
                .instrument_from_target()
                .push(svc::NewRouter::layer(RequestTarget::from))
                .into_inner();
            let http_detect =
                svc::stack(self.http_server(http, &metrics, span_sink.clone(), drain.clone()))
                    .push_cache(self.proxy.cache_max_idle_age)
                    .push(svc::NewUnwrapOr::layer(
                        svc::Fail::<_, NonOpaqueRefused>::default(),
                    ))
                    .push(detect::NewDetectService::timeout(
                        detect_timeout,
                        http::DetectHttp::default(),
                    ))
                    .into_inner();

            // If a transport header can be detected, use it to configure TCP
            // forwarding. If a transport header cannot be detected, try to
            // handle the connection as HTTP gateway traffic.
            tcp_forward
                .clone()
                .push_map_target(TcpEndpoint::from)
                // Update the TcpAccept target using a parsed transport-header.
                //
                // TODO use the transport header's `name` to inform gateway
                // routing.
                .push_map_target(|(h, mut t): (TransportHeader, TcpAccept)| {
                    t.target_addr = (t.target_addr.ip(), h.port).into();
                    t
                })
                // HTTP detection must *only* be performed when a transport
                // header is absent. When the header is present, we must
                // assume the protocol is opaque.
                .push(svc::NewUnwrapOr::layer(http_detect))
                .push(detect::NewDetectService::timeout(
                    detect_timeout,
                    DetectHeader::default(),
                ))
                // TODO this filter should actually extract the TLS status so
                // it's no longer wrapped in a conditional... i.e. proving to
                // the inner stack that the connection is secure.
                .push_request_filter(RequireIdentityForDirect)
                .push(metrics.transport.layer_accept())
                .push_map_target(TcpAccept::from)
                .push(tls::NewDetectTls::layer(
                    // TODO set ALPN on these connections to indicate support
                    // for the transport header.
                    local_identity.clone(),
                    self.proxy.detect_protocol_timeout,
                ))
                .check_new_service::<listen::Addrs, I>()
                .into_inner()
        };

        let http = self.http_router(connect, profiles_client, tap, &metrics, span_sink.clone());
        svc::stack(self.http_server(http, &metrics, span_sink, drain))
            .push(svc::NewUnwrapOr::layer(
                // When HTTP detection fails, forward the connection to the
                // application as an opaque TCP stream.
                tcp_forward
                    .clone()
                    .push_map_target(TcpEndpoint::from)
                    .into_inner(),
            ))
            .push_cache(self.proxy.cache_max_idle_age)
            .push(detect::NewDetectService::timeout(
                self.proxy.detect_protocol_timeout,
                http::DetectHttp::default(),
            ))
            .push_request_filter(self.require_identity_for_inbound_ports)
            .push(metrics.transport.layer_accept())
            .push_map_target(TcpAccept::from)
            .push(tls::NewDetectTls::layer(
                local_identity,
                self.proxy.detect_protocol_timeout,
            ))
            .push_switch(
                self.disable_protocol_detection_for_ports,
                tcp_forward
                    .push_map_target(TcpEndpoint::from)
                    .push(metrics.transport.layer_accept())
                    .push_map_target(TcpAccept::port_skipped)
                    .into_inner(),
            )
            .push_switch(prevent_loop, direct)
            .into_inner()
    }

    pub fn http_router<C, P>(
        &self,
        connect: C,
        profiles_client: P,
        tap: tap::Registry,
        metrics: &metrics::Proxy,
        span_sink: Option<mpsc::Sender<oc::Span>>,
    ) -> impl svc::NewService<
        TcpAccept,
        Service = impl svc::Service<
            http::Request<http::BoxBody>,
            Response = http::Response<http::BoxBody>,
            Error = Error,
            Future = impl Send,
        > + Clone,
    > + Clone
    where
        C: svc::Service<TcpEndpoint> + Clone + Send + Sync + Unpin + 'static,
        C::Response: io::AsyncRead + io::AsyncWrite + Send + Unpin + 'static,
        C::Error: Into<Error>,
        C::Future: Send + Unpin,
        P: profiles::GetProfile<NameAddr> + Clone + Send + Sync + 'static,
        P::Future: Send,
        P::Error: Send,
    {
        // Creates HTTP clients for each inbound port & HTTP settings.
        let endpoint = svc::stack(connect)
            .push(metrics.transport.layer_connect())
            .push_map_target(TcpEndpoint::from)
            .push(http::client::layer(
                self.proxy.connect.h1_settings,
                self.proxy.connect.h2_settings,
            ))
            .push(reconnect::layer({
                let backoff = self.proxy.connect.backoff;
                move |_| Ok(backoff.stream())
            }))
            .check_new_service::<HttpEndpoint, http::Request<_>>();

        let target = endpoint
            .push_map_target(HttpEndpoint::from)
            // Registers the stack to be tapped.
            .push(tap::NewTapHttp::layer(tap))
            // Records metrics for each `Target`.
            .push(metrics.http_endpoint.to_layer::<classify::Response, _>())
            .push_on_response(TraceContext::layer(
                span_sink.map(|span_sink| SpanConverter::client(span_sink, trace_labels())),
            ))
            .push_on_response(http::BoxResponse::layer())
            .check_new_service::<Target, http::Request<_>>();

        // Attempts to discover a service profile for each logical target (as
        // informed by the request's headers). The stack is cached until a
        // request has not been received for `cache_max_idle_age`.
        target
            .clone()
            .check_new_service::<Target, http::Request<http::BoxBody>>()
            .push_on_response(http::BoxRequest::layer())
            // The target stack doesn't use the profile resolution, so drop it.
            .push_map_target(Target::from)
            .push(profiles::http::route_request::layer(
                svc::proxies()
                    // Sets the route as a request extension so that it can be used
                    // by tap.
                    .push_http_insert_target()
                    // Records per-route metrics.
                    .push(metrics.http_route.to_layer::<classify::Response, _>())
                    // Sets the per-route response classifier as a request
                    // extension.
                    .push(classify::NewClassify::layer())
                    .check_new_clone::<dst::Route>()
                    .push_map_target(target::route)
                    .into_inner(),
            ))
            .push_map_target(Logical::from)
            .push(profiles::discover::layer(
                profiles_client,
                AllowProfile(self.allow_discovery.clone()),
            ))
            .push_on_response(http::BoxResponse::layer())
            .instrument(|_: &Target| debug_span!("profile"))
            // Skip the profile stack if it takes too long to become ready.
            .push_when_unready(target.clone(), self.profile_idle_timeout)
            .check_new_service::<Target, http::Request<http::BoxBody>>()
            .push_on_response(
                svc::layers()
                    .push(svc::layer::mk(svc::SpawnReady::new))
                    .push(svc::FailFast::layer(
                        "HTTP Logical",
                        self.proxy.dispatch_timeout,
                    ))
                    .push_spawn_buffer(self.proxy.buffer_capacity)
                    .push(metrics.stack.layer(stack_labels("http", "logical"))),
            )
            .push_cache(self.proxy.cache_max_idle_age)
            .push_on_response(
                svc::layers()
                    .push(http::Retain::layer())
                    .push(http::BoxResponse::layer()),
            )
            // Boxing is necessary purely to limit the link-time overhead of
            // having enormous types.
            .push(svc::BoxNewService::layer())
            .check_new_service::<Target, http::Request<http::BoxBody>>()
            // Removes the override header after it has been used to
            // determine a reuquest target.
            .push_on_response(strip_header::request::layer(DST_OVERRIDE_HEADER))
            // Routes each request to a target, obtains a service for that
            // target, and dispatches the request.
            .instrument_from_target()
            .push(svc::NewRouter::layer(RequestTarget::from))
            // Used by tap.
            .push_http_insert_target()
            .into_inner()
    }

    pub fn http_server<T, I, H, HSvc>(
        &self,
        http: H,
        metrics: &metrics::Proxy,
        span_sink: Option<mpsc::Sender<oc::Span>>,
        drain: drain::Watch,
    ) -> impl svc::NewService<
        (http::Version, T),
        Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send> + Clone,
    > + Clone
    where
        for<'t> &'t T: Into<SocketAddr>,
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + Send + Unpin + 'static,
        H: svc::NewService<T, Service = HSvc> + Clone + Send + Sync + Unpin + 'static,
        HSvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>
            + Clone
            + Send
            + Unpin
            + 'static,
        HSvc::Error: Into<Error>,
        HSvc::Future: Send,
    {
        let ProxyConfig {
            server: ServerConfig { h2_settings, .. },
            dispatch_timeout,
            max_in_flight_requests,
            ..
        } = self.proxy.clone();

        svc::stack(http)
            .check_new_service::<T, http::Request<_>>()
            .push_on_response(
                svc::layers()
                    // Downgrades the protocol if upgraded by an outbound proxy.
                    .push(orig_proto::Downgrade::layer())
                    // Limit the number of in-flight requests. When the proxy is
                    // at capacity, go into failfast after a dispatch timeout.
                    // Note that the inner service _always_ returns ready (due
                    // to `NewRouter`) and the concurrency limit need not be
                    // driven outside of the request path, so there's no need
                    // for SpawnReady
                    .push(svc::ConcurrencyLimit::layer(max_in_flight_requests))
                    .push(svc::FailFast::layer("HTTP Server", dispatch_timeout))
                    .push(metrics.http_errors.clone())
                    // Synthesizes responses for proxy errors.
                    .push(errors::layer())
                    .push(TraceContext::layer(span_sink.map(|span_sink| {
                        SpanConverter::server(span_sink, trace_labels())
                    })))
                    .push(metrics.stack.layer(stack_labels("http", "server")))
                    .push(http::BoxRequest::layer())
                    .push(http::BoxResponse::layer()),
            )
            .push(http::NewNormalizeUri::layer())
            .check_new_service::<T, http::Request<_>>()
            .push_map_target(|(_, t): (_, T)| t)
            .instrument(|(v, _): &(http::Version, _)| debug_span!("http", %v))
            .push(http::NewServeHttp::layer(h2_settings, drain))
            .into_inner()
    }
}

pub fn trace_labels() -> HashMap<String, String> {
    let mut l = HashMap::new();
    l.insert("direction".to_string(), "inbound".to_string());
    l
}

fn stack_labels(proto: &'static str, name: &'static str) -> metrics::StackLabels {
    metrics::StackLabels::inbound(proto, name)
}

// === impl SkipByPort ===

impl From<indexmap::IndexSet<u16>> for SkipByPort {
    fn from(ports: indexmap::IndexSet<u16>) -> Self {
        SkipByPort(ports.into())
    }
}

impl svc::stack::Switch<listen::Addrs> for SkipByPort {
    fn use_primary(&self, t: &listen::Addrs) -> bool {
        !self.0.contains(&t.target_addr().port())
    }
}

// === impl NonOpaqueRefused ===

impl Into<Error> for NonOpaqueRefused {
    fn into(self) -> Error {
        Error::from(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "Non-transport-header connection refused",
        ))
    }
}
