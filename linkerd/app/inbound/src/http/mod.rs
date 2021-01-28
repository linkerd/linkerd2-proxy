use crate::{
    allow_discovery::AllowProfile,
    target::{self, HttpEndpoint, Logical, RequestTarget, Target, TcpAccept, TcpEndpoint},
    Config,
};
pub use linkerd_app_core::proxy::http::{strip_header, BoxBody, DetectHttp, Request, Response};
use linkerd_app_core::{
    classify,
    config::{ProxyConfig, ServerConfig},
    drain, dst, errors, io, metrics,
    opencensus::proto::trace::v1 as oc,
    profiles,
    proxy::{http, tap},
    reconnect,
    spans::SpanConverter,
    svc, Error, NameAddr, TraceContext, DST_OVERRIDE_HEADER,
};
use std::net::SocketAddr;
use tokio::sync::mpsc;
use tracing::debug_span;

#[cfg(test)]
mod tests;

pub fn server<T, I, H, HSvc>(
    config: &ProxyConfig,
    http: H,
    metrics: &metrics::Proxy,
    span_sink: Option<mpsc::Sender<oc::Span>>,
    drain: drain::Watch,
) -> impl svc::NewService<
    (http::Version, T),
    Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send> + Clone,
> + Clone
where
    T: svc::stack::Param<SocketAddr>,
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
    } = config.clone();

    svc::stack(http)
        .check_new_service::<T, http::Request<_>>()
        // Convert origin form HTTP/1 URIs to absolute form for Hyper's
        // `Client`. This must be below the `orig_proto::Downgrade` layer, since
        // the request may have been downgraded from a HTTP/2 orig-proto request.
        .push(http::NewNormalizeUri::layer())
        .push_on_response(
            svc::layers()
                // Downgrades the protocol if upgraded by an outbound proxy.
                .push(http::orig_proto::Downgrade::layer())
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
                .push(TraceContext::layer(
                    span_sink.map(|k| SpanConverter::server(k, trace_labels())),
                ))
                .push(metrics.stack.layer(stack_labels("http", "server")))
                // Record when an HTTP/1 URI was in absolute form
                .push(http::normalize_uri::MarkAbsoluteForm::layer())
                .push(http::BoxRequest::layer())
                .push(http::BoxResponse::layer()),
        )
        .check_new_service::<T, http::Request<_>>()
        .push_map_target(|(_, t): (_, T)| t)
        .instrument(|(v, _): &(http::Version, _)| debug_span!("http", %v))
        .push(http::NewServeHttp::layer(h2_settings, drain))
        .into_inner()
}

pub fn router<C, P>(
    config: &Config,
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
            config.proxy.connect.h1_settings,
            config.proxy.connect.h2_settings,
        ))
        .push(reconnect::layer({
            let backoff = config.proxy.connect.backoff;
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
            span_sink.map(|k| SpanConverter::client(k, trace_labels())),
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
            AllowProfile(config.allow_discovery.clone()),
        ))
        .push_on_response(http::BoxResponse::layer())
        .instrument(|_: &Target| debug_span!("profile"))
        // Skip the profile stack if it takes too long to become ready.
        .push_on_response(svc::layer::mk(svc::SpawnReady::new))
        .push_when_unready(
            config.profile_idle_timeout,
            target
                .clone()
                .push_on_response(svc::layer::mk(svc::SpawnReady::new))
                .into_inner(),
        )
        .check_new_service::<Target, http::Request<http::BoxBody>>()
        .push_on_response(
            svc::layers()
                .push(svc::FailFast::layer(
                    "HTTP Logical",
                    config.proxy.dispatch_timeout,
                ))
                .push_spawn_buffer(config.proxy.buffer_capacity)
                .push(metrics.stack.layer(stack_labels("http", "logical"))),
        )
        .push_cache(config.proxy.cache_max_idle_age)
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
        // determine a request target.
        .push_on_response(strip_header::request::layer(DST_OVERRIDE_HEADER))
        // Routes each request to a target, obtains a service for that
        // target, and dispatches the request.
        .instrument_from_target()
        .push(svc::NewRouter::layer(RequestTarget::from))
        // Used by tap.
        .push_http_insert_target()
        .into_inner()
}

fn trace_labels() -> std::collections::HashMap<String, String> {
    let mut l = std::collections::HashMap::new();
    l.insert("direction".to_string(), "inbound".to_string());
    l
}

fn stack_labels(proto: &'static str, name: &'static str) -> metrics::StackLabels {
    metrics::StackLabels::inbound(proto, name)
}
