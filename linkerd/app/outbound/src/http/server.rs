use crate::{http, trace_labels, Outbound};
use linkerd_app_core::{config, errors, http_tracing, svc, Error};
use tracing::debug_span;

impl<H, HSvc> Outbound<H>
where
    H: svc::NewService<http::Logical, Service = HSvc> + Clone + Send + 'static,
    HSvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>,
    HSvc: Send + 'static,
    HSvc::Error: Into<Error>,
    HSvc::Future: Send,
{
    pub fn push_http_server(
        self,
    ) -> Outbound<
        impl svc::NewService<
                http::Logical,
                Service = impl svc::Service<
                    http::Request<http::BoxBody>,
                    Response = http::Response<http::BoxBody>,
                    Error = Error,
                    Future = impl Send,
                > + Clone,
            > + Clone,
    > {
        let Self {
            config,
            runtime: rt,
            stack: http,
        } = self;

        let config::ProxyConfig {
            dispatch_timeout,
            max_in_flight_requests,
            buffer_capacity,
            ..
        } = config.proxy;

        let stack = http
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
                    .push(rt.metrics.http_errors.clone())
                    // Synthesizes responses for proxy errors.
                    .push(errors::layer())
                    // Initiates OpenCensus tracing.
                    .push(http_tracing::server(rt.span_sink.clone(), trace_labels()))
                    .push(http::BoxResponse::layer()),
            )
            // Convert origin form HTTP/1 URIs to absolute form for Hyper's
            // `Client`.
            .push(http::NewNormalizeUri::layer())
            // Record when a HTTP/1 URI originated in absolute form
            .push_on_response(http::normalize_uri::MarkAbsoluteForm::layer())
            .instrument(|l: &http::Logical| debug_span!("http", v = %l.protocol))
            .check_new_service::<http::Logical, http::Request<http::BoxBody>>();

        Outbound {
            config,
            runtime: rt,
            stack,
        }
    }
}
