use crate::{http, trace_labels};
use linkerd_app_core::{
    config::ProxyConfig, errors, metrics, opencensus::proto::trace::v1 as oc, spans::SpanConverter,
    svc, Error, TraceContext,
};
use tokio::sync::mpsc;
use tracing::debug_span;

pub fn stack<H, HSvc>(
    config: &ProxyConfig,
    metrics: &metrics::Proxy,
    span_sink: Option<mpsc::Sender<oc::Span>>,
    http: H,
) -> impl svc::NewService<
    http::Logical,
    Service = impl svc::Service<
        http::Request<http::BoxBody>,
        Response = http::Response<http::BoxBody>,
        Error = Error,
        Future = impl Send,
    > + Clone,
> + Clone
where
    H: svc::NewService<http::Logical, Service = HSvc> + Clone + Send + 'static,
    HSvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>
        + Send
        + 'static,
    HSvc::Error: Into<Error>,
    HSvc::Future: Send,
{
    let ProxyConfig {
        dispatch_timeout,
        max_in_flight_requests,
        buffer_capacity,
        ..
    } = config.clone();

    svc::stack(http)
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
                .push(http::BoxResponse::layer()),
        )
        // Convert origin form HTTP/1 URIs to absolute form for Hyper's
        // `Client`.
        .push(http::NewNormalizeUri::layer())
        // Record when a HTTP/1 URI originated in absolute form
        .push_on_response(http::normalize_uri::MarkAbsoluteForm::layer())
        .instrument(|l: &http::Logical| debug_span!("http", v = %l.protocol))
        .check_new_service::<http::Logical, http::Request<http::BoxBody>>()
        .into_inner()
}
