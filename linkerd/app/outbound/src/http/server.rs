use super::{peer_proxy_errors::PeerProxyErrors, IdentityRequired};
use crate::{http, trace_labels, Outbound};
use linkerd_app_core::{config, errors, http_tracing, svc, Error, Result};

#[derive(Copy, Clone, Debug)]
pub(crate) struct ServerRescue;

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
                        .push(PeerProxyErrors::layer())
                        // Synthesizes responses for proxy errors.
                        .push(ServerRescue::layer())
                        // Initiates OpenCensus tracing.
                        .push(http_tracing::server(rt.span_sink.clone(), trace_labels()))
                        .push(http::BoxResponse::layer()),
                )
                // Convert origin form HTTP/1 URIs to absolute form for Hyper's
                // `Client`.
                .push(http::NewNormalizeUri::layer())
                // Record when a HTTP/1 URI originated in absolute form
                .push_on_service(http::normalize_uri::MarkAbsoluteForm::layer())
                .check_new_service::<T, http::Request<http::BoxBody>>()
                .push(svc::ArcNewService::layer())
        })
    }
}

// === impl ServerRescue ===

impl ServerRescue {
    pub fn layer() -> errors::respond::Layer<Self> {
        errors::respond::NewRespond::layer(Self)
    }
}

impl errors::HttpRescue<Error> for ServerRescue {
    fn rescue(&self, error: Error) -> Result<errors::SyntheticHttpResponse> {
        let cause = errors::root_cause(&*error);
        if cause.is::<http::ResponseTimeoutError>() {
            return Ok(errors::SyntheticHttpResponse::gateway_timeout(cause));
        }
        if cause.is::<IdentityRequired>() {
            return Ok(errors::SyntheticHttpResponse::bad_gateway(cause));
        }
        if cause.is::<errors::FailFastError>() {
            return Ok(errors::SyntheticHttpResponse::gateway_timeout(cause));
        }

        if cause.is::<errors::H2Error>() {
            return Err(error);
        }

        tracing::warn!(%error, "Unexpected error");
        Ok(errors::SyntheticHttpResponse::unexpected_error())
    }
}
