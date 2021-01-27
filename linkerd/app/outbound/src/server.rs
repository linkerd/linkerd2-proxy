#![allow(clippy::too_many_arguments)]

use crate::{http, stack_labels, target::ShouldResolve, tcp, trace_labels, Config};
use linkerd_app_core::{
    config::{ProxyConfig, ServerConfig},
    detect, discovery_rejected, drain, errors, io, metrics,
    opencensus::proto::trace::v1 as oc,
    profiles,
    proxy::{api_resolve::Metadata, core::resolve::Resolve},
    spans::SpanConverter,
    svc, tls,
    transport::{listen, metrics::SensorIo},
    Addr, Error, IpMatch, TraceContext,
};
use std::net::SocketAddr;
use tokio::sync::mpsc;
use tracing::debug_span;

pub fn stack<R, P, C, H, HSvc, I>(
    config: &Config,
    profiles: P,
    resolve: R,
    tcp_connect: C,
    http_router: H,
    metrics: metrics::Proxy,
    span_sink: Option<mpsc::Sender<oc::Span>>,
    drain: drain::Watch,
) -> impl svc::NewService<
    listen::Addrs,
    Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send>,
>
where
    I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + std::fmt::Debug + Send + Unpin + 'static,
    R: Resolve<Addr, Endpoint = Metadata, Error = Error> + Clone + Send + 'static,
    R::Resolution: Send,
    R::Future: Send,
    C: svc::Service<tcp::Endpoint> + Clone + Send + 'static,
    C::Response: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin,
    C::Error: Into<Error>,
    C::Future: Send,
    H: svc::NewService<http::Logical, Service = HSvc> + Clone + Send + 'static,
    HSvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>
        + Send
        + 'static,
    HSvc::Error: Into<Error>,
    HSvc::Future: Send,
    P: profiles::GetProfile<SocketAddr> + Clone + Send + 'static,
    P::Future: Send,
    P::Error: Send,
{
    let tcp_balance =
        tcp::balance::stack(&config.proxy, tcp_connect.clone(), resolve, drain.clone());
    let accept = accept_stack(
        config,
        profiles,
        tcp_connect,
        tcp_balance,
        http_router,
        metrics.clone(),
        span_sink,
        drain,
    );
    cache(&config.proxy, metrics, accept)
}

/// Wraps an `N`-typed TCP stack with caching.
///
/// Services are cached as long as they are retained by the caller (usually
/// core::serve) and are evicted from the cache when they have been dropped for
/// `config.cache_max_idle_age` with no new acquisitions.
pub fn cache<N, NSvc, I>(
    config: &ProxyConfig,
    metrics: metrics::Proxy,
    stack: N,
) -> impl svc::NewService<
    listen::Addrs,
    Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send>,
>
where
    I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + std::fmt::Debug + Send + Unpin + 'static,
    N: svc::NewService<tcp::Accept, Service = NSvc> + 'static,
    NSvc: svc::Service<SensorIo<I>, Response = ()> + Send + 'static,
    NSvc::Error: Into<Error>,
    NSvc::Future: Send,
{
    svc::stack(stack)
        .push_on_response(
            svc::layers()
                // If the traffic split is empty/unavailable, eagerly fail
                // requests requests. When the split is in failfast, spawn
                // the service in a background task so it becomes ready without
                // new requests.
                .push(svc::layer::mk(svc::SpawnReady::new))
                .push(svc::FailFast::layer("TCP Server", config.dispatch_timeout))
                .push_spawn_buffer(config.buffer_capacity),
        )
        .push_cache(config.cache_max_idle_age)
        .push(metrics.transport.layer_accept())
        .push_map_target(tcp::Accept::from)
        .into_inner()
}

pub fn accept_stack<P, C, T, TSvc, H, HSvc, I>(
    config: &Config,
    profiles: P,
    tcp_connect: C,
    tcp_balance: T,
    http_router: H,
    metrics: metrics::Proxy,
    span_sink: Option<mpsc::Sender<oc::Span>>,
    drain: drain::Watch,
) -> impl svc::NewService<
    tcp::Accept,
    Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send>,
> + Clone
where
    I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + std::fmt::Debug + Send + Unpin + 'static,
    C: svc::Service<tcp::Endpoint> + Clone + Send + 'static,
    C::Response: io::AsyncRead + io::AsyncWrite + Send + Unpin,
    C::Error: Into<Error>,
    C::Future: Send,
    T: svc::NewService<tcp::Concrete, Service = TSvc> + Clone + Send + 'static,
    TSvc: svc::Service<io::PrefixedIo<I>, Response = ()>
        + svc::Service<I, Response = ()>
        + Send
        + 'static,
    <TSvc as svc::Service<I>>::Error: Into<Error>,
    <TSvc as svc::Service<I>>::Future: Send,
    <TSvc as svc::Service<io::PrefixedIo<I>>>::Error: Into<Error>,
    <TSvc as svc::Service<io::PrefixedIo<I>>>::Future: Send,
    H: svc::NewService<http::Logical, Service = HSvc> + Clone + Send + 'static,
    HSvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>
        + Send
        + 'static,
    HSvc::Error: Into<Error>,
    HSvc::Future: Send,
    P: profiles::GetProfile<SocketAddr> + Clone + Send + 'static,
    P::Future: Send,
    P::Error: Send,
{
    let ProxyConfig {
        server: ServerConfig { h2_settings, .. },
        dispatch_timeout,
        max_in_flight_requests,
        detect_protocol_timeout,
        buffer_capacity,
        cache_max_idle_age,
        ..
    } = config.proxy.clone();

    let tcp_forward = svc::stack(tcp_connect)
        .push_make_thunk()
        .push_on_response(tcp::Forward::layer())
        .push(svc::MapErrLayer::new(Into::into))
        .into_new_service()
        .push_on_response(metrics.stack.layer(stack_labels("tcp", "forward")))
        .push_map_target(tcp::Endpoint::from_logical(
            tls::NoClientTls::NotProvidedByServiceDiscovery,
        ))
        .into_inner();

    svc::stack(http_router)
        .check_new_service::<http::Logical, _>()
        .push_on_response(
            svc::layers()
                .push(http::BoxRequest::layer())
                // Limit the number of in-flight requests. When the proxy is
                // at capacity, go into failfast after a dispatch timeout. If
                // the router is unavailable, then spawn the service on a
                // background task to ensure it becomes ready without new
                // requests being processed.
                .push(svc::layer::mk(svc::SpawnReady::new))
                .push(svc::ConcurrencyLimit::layer(max_in_flight_requests))
                .push(svc::FailFast::layer("HTTP Server", dispatch_timeout))
                .push_spawn_buffer(buffer_capacity)
                .push(metrics.http_errors.clone())
                // Synthesizes responses for proxy errors.
                .push(errors::layer())
                // Initiates OpenCensus tracing.
                .push(TraceContext::layer(span_sink.map(|span_sink| {
                    SpanConverter::server(span_sink, trace_labels())
                })))
                .push(metrics.stack.layer(stack_labels("http", "server")))
                .push(http::BoxResponse::layer()),
        )
        .check_new_service::<http::Logical, _>()
        .push(http::NewNormalizeUri::layer())
        .instrument(|l: &http::Logical| debug_span!("http", v = %l.protocol))
        .push_map_target(http::Logical::from)
        .push(http::NewServeHttp::layer(h2_settings, drain))
        .push(svc::UnwrapOr::layer(
            // When an HTTP version cannot be detected, we fallback to a logical
            // TCP stack. This service needs to be buffered so that it can be
            // cached and cloned per connection.
            svc::stack(tcp_balance.clone())
                .push_map_target(tcp::Concrete::from)
                .push(profiles::split::layer())
                .push_switch(ShouldResolve, tcp_forward.clone())
                .push_on_response(
                    svc::layers()
                        .push(svc::layer::mk(svc::SpawnReady::new))
                        .push(svc::FailFast::layer("TCP Logical", dispatch_timeout))
                        .push_spawn_buffer(buffer_capacity)
                        .push(metrics.stack.layer(stack_labels("tcp", "logical"))),
                )
                .instrument(|_: &_| debug_span!("tcp"))
                .check_new_service::<tcp::Logical, _>()
                .into_inner(),
        ))
        .push_cache(cache_max_idle_age)
        .push(detect::NewDetectService::timeout(
            detect_protocol_timeout,
            http::DetectHttp::default(),
        ))
        .check_new_service::<tcp::Logical, _>()
        .push_switch(
            SkipByProfile,
            // When the profile marks the target as opaque, we skip HTTP
            // detection and just use the TCP logical stack directly. Unlike the
            // above case, this stack need not be buffered, since `fn cache`
            // applies its own buffer on the returned service.
            svc::stack(tcp_balance)
                .push_map_target(tcp::Concrete::from)
                .push(profiles::split::layer())
                .push_switch(ShouldResolve, tcp_forward)
                .push_on_response(metrics.stack.layer(stack_labels("tcp", "passthru")))
                .instrument(|_: &_| debug_span!("tcp.opaque"))
                .into_inner(),
        )
        .check_new_service::<tcp::Logical, _>()
        .push_map_target(tcp::Logical::from)
        .push(profiles::discover::layer(
            profiles,
            AllowProfile(config.allow_discovery.clone().into()),
        ))
        .check_new_service::<tcp::Accept, _>()
        .into_inner()
}

#[derive(Clone, Debug)]
pub struct AllowProfile(pub IpMatch);

// === impl AllowProfile ===

impl svc::stack::Predicate<tcp::Accept> for AllowProfile {
    type Request = std::net::SocketAddr;

    fn check(&mut self, a: tcp::Accept) -> Result<std::net::SocketAddr, Error> {
        if self.0.matches(a.orig_dst.ip()) {
            Ok(a.orig_dst)
        } else {
            Err(discovery_rejected().into())
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct SkipByProfile;

// === impl SkipByProfile ===

impl svc::Predicate<tcp::Logical> for SkipByProfile {
    type Request = svc::Either<tcp::Logical, tcp::Logical>;

    fn check(&mut self, l: tcp::Logical) -> Result<Self::Request, Error> {
        if l.profile
            .as_ref()
            .map(|p| !p.borrow().opaque_protocol)
            .unwrap_or(true)
        {
            Ok(svc::Either::A(l))
        } else {
            Ok(svc::Either::B(l))
        }
    }
}
