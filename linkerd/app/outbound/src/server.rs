#![allow(clippy::too_many_arguments)]

use crate::{http, stack_labels, tcp, trace_labels, Config};
use linkerd2_app_core::{
    config::{ProxyConfig, ServerConfig},
    discovery_rejected, drain, errors, metrics,
    opencensus::proto::trace::v1 as oc,
    profiles,
    proxy::{api_resolve::Metadata, core::resolve::Resolve},
    spans::SpanConverter,
    svc,
    transport::{self, io, listen, tls},
    Addr, Error, IpMatch, TraceContext,
};
use std::net::SocketAddr;
use tokio::sync::mpsc;
use tracing::debug_span;

pub fn stack<R, P, C, H, S, I>(
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
    I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + std::fmt::Debug + Unpin + Send + 'static,
    R: Resolve<Addr, Endpoint = Metadata, Error = Error> + Unpin + Clone + Send + Sync + 'static,
    R::Future: Unpin + Send,
    R::Resolution: Unpin + Send,
    C: tower::Service<tcp::Endpoint, Error = Error> + Unpin + Clone + Send + Sync + 'static,
    C::Response: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    C::Future: Unpin + Send,
    H: svc::NewService<http::Logical, Service = S> + Unpin + Clone + Send + Sync + 'static,
    S: tower::Service<
            http::Request<http::boxed::BoxBody>,
            Response = http::Response<http::boxed::BoxBody>,
            Error = Error,
        > + Send
        + 'static,
    S::Future: Send,
    P: profiles::GetProfile<SocketAddr> + Unpin + Clone + Send + Sync + 'static,
    P::Future: Unpin + Send,
    P::Error: Send,
{
    let tcp_balance = tcp::balance::stack(&config.proxy, tcp_connect.clone(), resolve);
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
pub fn cache<N, S, I>(
    config: &ProxyConfig,
    metrics: metrics::Proxy,
    stack: N,
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
    I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + std::fmt::Debug + Unpin + Send + 'static,
    transport::metrics::SensorIo<I>: Send + 'static,
    N: svc::NewService<tcp::Accept, Service = S> + Clone + Send + 'static,
    S: svc::Service<transport::metrics::SensorIo<I>, Response = ()> + Send + 'static,
    S::Error: Into<Error> + Send + 'static,
    S::Future: Send + 'static,
{
    svc::stack(stack)
        .push_on_response(
            svc::layers()
                .push_failfast(config.dispatch_timeout)
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
    Service = impl tower::Service<
        transport::metrics::SensorIo<I>,
        Response = (),
        Error = impl Into<Error>,
        Future = impl Send + 'static,
    > + Send
                  + 'static,
> + Clone
       + Send
       + 'static
where
    I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + std::fmt::Debug + Unpin + Send + 'static,
    C: tower::Service<tcp::Endpoint, Error = Error> + Unpin + Clone + Send + Sync + 'static,
    C::Response: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    C::Future: Unpin + Send,
    T: svc::NewService<tcp::Concrete, Service = TSvc> + Clone + Unpin + Send + 'static,
    TSvc: tower::Service<
            transport::io::PrefixedIo<transport::metrics::SensorIo<I>>,
            Response = (),
            Error = Error,
        > + Unpin
        + Send
        + 'static,
    <TSvc as tower::Service<transport::io::PrefixedIo<transport::metrics::SensorIo<I>>>>::Future:
        Unpin + Send + 'static,
    TSvc: tower::Service<transport::metrics::SensorIo<I>, Response = (), Error = Error>
        + Unpin
        + Send
        + 'static,
    <TSvc as tower::Service<transport::metrics::SensorIo<I>>>::Future: Unpin + Send + 'static,
    H: svc::NewService<http::Logical, Service = HSvc> + Unpin + Clone + Send + Sync + 'static,
    HSvc: tower::Service<
            http::Request<http::boxed::BoxBody>,
            Response = http::Response<http::boxed::BoxBody>,
            Error = Error,
        > + Send
        + 'static,
    HSvc::Future: Send,
    P: profiles::GetProfile<SocketAddr> + Unpin + Clone + Send + Sync + 'static,
    P::Future: Unpin + Send,
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

    let http_server = svc::stack(http_router)
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
                .push(TraceContext::layer(span_sink.map(|span_sink| {
                    SpanConverter::server(span_sink, trace_labels())
                })))
                .push(metrics.stack.layer(stack_labels("http", "server")))
                .push_failfast(dispatch_timeout)
                .push_spawn_buffer(buffer_capacity)
                .box_http_response(),
        )
        .push(svc::layer::mk(http::normalize_uri::MakeNormalizeUri::new))
        .instrument(|l: &http::Logical| debug_span!("http", v = %l.protocol))
        .push_map_target(http::Logical::from)
        .into_inner();

    let tcp_forward = svc::stack(tcp_connect.clone())
        .push_make_thunk()
        .push_on_response(svc::layer::mk(tcp::Forward::new))
        .into_new_service()
        .push_on_response(metrics.stack.layer(stack_labels("tcp", "forward")))
        .push_map_target(tcp::Endpoint::from_logical(
            tls::ReasonForNoPeerName::NotProvidedByServiceDiscovery,
        ))
        .into_inner();

    // Load balances TCP streams that cannot be decoded as HTTP.
    let tcp_balance = svc::stack(tcp_balance)
        .push_map_target(tcp::Concrete::from)
        .push(profiles::split::layer())
        .push_switch(tcp::Logical::should_resolve, tcp_forward)
        .push_on_response(
            svc::layers()
                .push_failfast(dispatch_timeout)
                .push_spawn_buffer(buffer_capacity),
        )
        .instrument(|_: &_| debug_span!("tcp"))
        .into_inner();

    let http = svc::stack(http::NewServeHttp::new(h2_settings, http_server, drain))
        .push(svc::stack::NewOptional::layer(tcp_balance))
        .push_cache(cache_max_idle_age)
        .push(transport::NewDetectService::layer(
            transport::detect::DetectTimeout::new(
                detect_protocol_timeout,
                http::DetectHttp::default(),
            ),
        ))
        .into_inner();

    let tcp = svc::stack(tcp::connect::forward(tcp_connect))
        .push_map_target(tcp::Endpoint::from_logical(
            tls::ReasonForNoPeerName::PortSkipped,
        ))
        .push_on_response(metrics.stack.layer(stack_labels("tcp", "opaque")))
        .into_inner();

    svc::stack(http)
        .push_switch(SkipByProfile, tcp)
        .push_map_target(tcp::Logical::from)
        .push(profiles::discover::layer(
            profiles,
            AllowProfile(config.allow_discovery.clone().into()),
        ))
        .into_inner()
}

#[derive(Clone, Debug)]
pub struct AllowProfile(pub IpMatch);

// === impl AllowProfile ===

impl svc::stack::FilterRequest<tcp::Accept> for AllowProfile {
    type Request = std::net::SocketAddr;

    fn filter(&self, a: tcp::Accept) -> Result<std::net::SocketAddr, Error> {
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

impl svc::stack::Switch<tcp::Logical> for SkipByProfile {
    fn use_primary(&self, l: &tcp::Logical) -> bool {
        l.profile
            .as_ref()
            .map(|p| !p.borrow().opaque_protocol)
            .unwrap_or(true)
    }
}
