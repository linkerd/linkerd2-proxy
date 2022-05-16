use super::{IdentityRequired, ProxyConnectionClose};
use crate::{http, trace_labels, Outbound};
use linkerd_app_core::{
    config, errors, http_tracing,
    svc::{self, ExtractParam},
    Error, Result,
};

#[derive(Copy, Clone, Debug)]
pub(crate) struct ServerRescue {
    emit_headers: bool,
}

impl<N> Outbound<N> {
    pub fn push_http_server<T, NSvc>(
        self,
    ) -> Outbound<
        svc::ArcNewService<
            T,
            impl svc::Service<
                    http::Request<http::BoxBody>,
                    Response = http::Response<http::BoxBody>,
                    Error = Error,
                    Future = impl Send,
                > + Clone,
        >,
    >
    where
        T: svc::Param<http::normalize_uri::DefaultAuthority>,
        N: svc::NewService<T, Service = NSvc> + Clone + Send + Sync + 'static,
        NSvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>,
        NSvc: Send + 'static,
        NSvc::Error: Into<Error>,
        NSvc::Future: Send,
    {
        self.map_stack(|config, rt, http| {
            let config::ProxyConfig {
                dispatch_timeout,
                max_in_flight_requests,
                buffer_capacity,
                ..
            } = config.proxy;

            http.check_new_service::<T, _>()
                .push_on_service(
                    svc::layers()
                        .push(http::BoxRequest::layer())
                        // Limit the number of in-flight requests. When the proxy is
                        // at capacity, go into failfast after a dispatch timeout. If
                        // the router is unavailable, then spawn the service on a
                        // background task to ensure it becomes ready without new
                        // requests being processed.
                        .push(svc::layer::mk(svc::SpawnReady::new))
                        .push(svc::ConcurrencyLimitLayer::new(max_in_flight_requests))
                        .push(svc::FailFast::layer("HTTP Server", dispatch_timeout))
                        .push_spawn_buffer(buffer_capacity)
                        .push(rt.metrics.http_errors.to_layer())
                        // Tear down server connections when a peer proxy generates an error.
                        .push(ProxyConnectionClose::layer()),
                )
                // Synthesizes responses for proxy errors.
                .check_new_service::<T, http::Request<_>>()
                .push(ServerRescue::layer(config.emit_headers))
                .check_new_service::<T, http::Request<_>>()
                .push_on_service(
                    svc::layers()
                        // Initiates OpenCensus tracing.
                        .push(http_tracing::server(rt.span_sink.clone(), trace_labels()))
                        .push(http::BoxResponse::layer()),
                )
                // Convert origin form HTTP/1 URIs to absolute form for Hyper's
                // `Client`.
                .push(http::NewNormalizeUri::layer())
                // Record when a HTTP/1 URI originated in absolute form
                .push_on_service(http::normalize_uri::MarkAbsoluteForm::layer())
                .push(svc::ArcNewService::layer())
        })
    }
}

// === impl ServerRescue ===

impl ServerRescue {
    pub fn layer<N>(
        emit_headers: bool,
    ) -> impl svc::layer::Layer<N, Service = errors::NewRespondService<Self, Self, N>> + Clone {
        errors::respond::layer(Self { emit_headers })
    }
}

impl<T> ExtractParam<Self, T> for ServerRescue {
    #[inline]
    fn extract_param(&self, _: &T) -> Self {
        *self
    }
}

impl<T> ExtractParam<errors::respond::EmitHeaders, T> for ServerRescue {
    #[inline]
    fn extract_param(&self, _: &T) -> errors::respond::EmitHeaders {
        errors::respond::EmitHeaders(self.emit_headers)
    }
}

impl errors::HttpRescue<Error> for ServerRescue {
    fn rescue(&self, error: Error) -> Result<errors::SyntheticHttpResponse> {
        if let Some(cause) = errors::cause_ref::<http::ResponseTimeoutError>(&*error) {
            return Ok(errors::SyntheticHttpResponse::gateway_timeout(cause));
        }
        if let Some(cause) = errors::cause_ref::<IdentityRequired>(&*error) {
            return Ok(errors::SyntheticHttpResponse::bad_gateway(cause));
        }
        if let Some(cause) = errors::cause_ref::<errors::FailFastError>(&*error) {
            return Ok(errors::SyntheticHttpResponse::gateway_timeout(cause));
        }

        if errors::is_caused_by::<errors::H2Error>(&*error) {
            return Err(error);
        }

        tracing::warn!(error, "Unexpected error");
        Ok(errors::SyntheticHttpResponse::unexpected_error())
    }
}
