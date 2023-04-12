use super::IdentityRequired;
use crate::{http, trace_labels, Outbound};
use linkerd_app_core::{
    errors, http_tracing, io,
    svc::{self, ExtractParam},
    Error, Result,
};

#[derive(Copy, Clone, Debug)]
pub(crate) struct ServerRescue {
    emit_headers: bool,
}

impl<N> Outbound<N> {
    /// Builds a [`svc::NewService`] stack that prepares HTTP requests to be
    /// proxied.
    ///
    /// The services produced by this stack handle errors, converting them into
    /// HTTP responses.
    ///
    /// Inner services must be available, otherwise load shedding is applied.
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
        // Target
        T: svc::Param<http::normalize_uri::DefaultAuthority>,
        // HTTP outbound stack
        N: svc::NewService<T, Service = NSvc> + Clone + Send + Sync + 'static,
        NSvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>,
        NSvc: Clone + Send + 'static,
        NSvc::Error: Into<Error>,
        NSvc::Future: Send,
    {
        self.map_stack(|config, rt, http| {
            http.check_new_service::<T, _>()
                .push_on_service(
                    svc::layers()
                        // Limit the number of in-flight outbound requests
                        // (across all targets).
                        //
                        // TODO(ver) This concurrency limit applies only to
                        // requests that do not yet have responses, but ignores
                        // streaming bodies. We should change this to an
                        // HTTP-specific imlementation that tracks request and
                        // response bodies.
                        .push(svc::ConcurrencyLimitLayer::new(
                            config.proxy.max_in_flight_requests,
                        ))
                        // Shed load by failing requests when the concurrency
                        // limit is reached or the inner service is otherwise
                        // not ready for requests.
                        .push(svc::LoadShed::layer())
                        .push(rt.metrics.http_errors.to_layer()),
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

    pub fn push_tcp_http_server<T, I, NSvc>(
        self,
    ) -> Outbound<http::NewServeHttp<svc::ArcNewService<T, svc::NewCloneService<NSvc>>>>
    where
        // Target
        T: svc::Param<http::Version>,
        T: Clone + Send + Unpin + 'static,
        // Server-side socket
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + Send + Unpin + 'static,
        // Inner stack
        N: svc::NewService<T, Service = NSvc> + Clone + Send + Sync + Unpin + 'static,
        NSvc: svc::Service<
            http::Request<http::BoxBody>,
            Response = http::Response<http::BoxBody>,
            Error = Error,
        >,
        NSvc: Clone + Send + Unpin + 'static,
        NSvc::Future: Send,
    {
        self.map_stack(|config, rt, http| {
            http.unlift_new()
                .push(svc::ArcNewService::layer())
                .push(http::NewServeHttp::layer(
                    config.proxy.server.h2_settings,
                    rt.drain.clone(),
                ))
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
        use super::logical::policy::errors as policy;

        // A profile configured request timeout was encountered.
        if errors::is_caused_by::<http::ResponseTimeoutError>(&*error) {
            return Ok(errors::SyntheticHttpResponse::gateway_timeout(error));
        }

        // A request with a `l5d-require-id` header are dispatched to endpoints
        // with a different identity.
        if errors::is_caused_by::<IdentityRequired>(&*error) {
            return Ok(errors::SyntheticHttpResponse::bad_gateway(error));
        }

        // No available backend can be found for a request.
        if errors::is_caused_by::<errors::FailFastError>(&*error) {
            // XXX(ver) This should probably be SERVICE_UNAVAILABLE, because
            // this is basically no different from a LoadShedError, but that
            // would be a change in behavior.
            return Ok(errors::SyntheticHttpResponse::gateway_timeout(error));
        }
        if errors::is_caused_by::<errors::LoadShedError>(&*error) {
            return Ok(errors::SyntheticHttpResponse::unavailable(error));
        }
        if errors::is_caused_by::<super::concrete::DispatcherFailed>(&*error) {
            return Ok(errors::SyntheticHttpResponse::bad_gateway(error));
        }

        // No routes configured for a request.
        if errors::is_caused_by::<super::logical::NoRoute>(&*error) {
            return Ok(errors::SyntheticHttpResponse::not_found(error));
        }

        // Policy-driven request redirection.
        if let Some(policy::HttpRouteRedirect { status, location }) = errors::cause_ref(&*error) {
            return Ok(errors::SyntheticHttpResponse::redirect(*status, location));
        }

        // Policy-driven request failures.
        if let Some(policy::HttpRouteInjectedFailure { status, message }) =
            errors::cause_ref(&*error)
        {
            return Ok(errors::SyntheticHttpResponse::response(
                *status,
                message.to_string(),
            ));
        }
        if let Some(policy::GrpcRouteInjectedFailure { code, message }) = errors::cause_ref(&*error)
        {
            return Ok(errors::SyntheticHttpResponse::grpc(
                (*code as i32).into(),
                message.to_string(),
            ));
        }

        // HTTP/2 errors.
        if errors::is_caused_by::<errors::H2Error>(&*error) {
            return Err(error);
        }

        // Everything else...
        tracing::warn!(error, "Unexpected error");
        Ok(errors::SyntheticHttpResponse::unexpected_error())
    }
}
