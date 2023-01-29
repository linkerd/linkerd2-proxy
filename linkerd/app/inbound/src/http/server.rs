use super::set_identity_header::NewSetIdentityHeader;
use crate::{policy, Inbound};
pub use linkerd_app_core::proxy::http::{
    normalize_uri, strip_header, uri, BoxBody, BoxResponse, DetectHttp, Request, Response, Retain,
    Version,
};
use linkerd_app_core::{
    config::{ProxyConfig, ServerConfig},
    errors, http_tracing, io,
    metrics::ServerLabel,
    proxy::http,
    svc::{self, ExtractParam, Param},
    tls,
    transport::{ClientAddr, OrigDstAddr, Remote},
    Error, Result,
};
use linkerd_http_access_log::NewAccessLog;
use tracing::debug_span;

#[derive(Copy, Clone, Debug)]
struct ServerRescue;

impl<H> Inbound<H> {
    /// Fails requests when the `HSvc`-typed inner service is not ready.
    pub fn push_http_server<T, I, HSvc>(self) -> Inbound<svc::ArcNewTcp<T, I>>
    where
        T: Param<Version>
            + Param<http::normalize_uri::DefaultAuthority>
            + Param<tls::ConditionalServerTls>
            + Param<ServerLabel>
            + Param<OrigDstAddr>
            + Param<Remote<ClientAddr>>,
        T: Clone + Send + Unpin + 'static,
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
        self.map_stack(|config, rt, http| {
            let ProxyConfig {
                server: ServerConfig { h2_settings, .. },
                max_in_flight_requests,
                ..
            } = config.proxy;

            http.check_new_service::<T, http::Request<_>>()
                // Convert origin form HTTP/1 URIs to absolute form for Hyper's
                // `Client`. This must be below the `orig_proto::Downgrade` layer, since
                // the request may have been downgraded from a HTTP/2 orig-proto request.
                .push(http::NewNormalizeUri::layer())
                .push(NewSetIdentityHeader::layer(()))
                .push_on_service(
                    svc::layers()
                        .push(http::BoxRequest::layer())
                        // Downgrades the protocol if upgraded by an outbound proxy.
                        .push(http::orig_proto::Downgrade::layer())
                        // Limit the number of in-flight inbound requests.
                        //
                        // TODO(ver) This concurrency limit applies only to
                        // requests that do not yet have responses, but ignores
                        // streaming bodies. We should change this to an
                        // HTTP-specific imlementation that tracks request and
                        // response bodies.
                        .push(svc::ConcurrencyLimitLayer::new(max_in_flight_requests))
                        // Shed load by failing requests when the concurrency
                        // limit is reached.
                        .push(svc::LoadShed::layer()),
                )
                .push(rt.metrics.http_errors.to_layer())
                .push(ServerRescue::layer())
                .push_on_service(
                    svc::layers()
                        .push(http_tracing::server(
                            rt.span_sink.clone(),
                            super::trace_labels(),
                        ))
                        // Record when an HTTP/1 URI was in absolute form
                        .push(http::normalize_uri::MarkAbsoluteForm::layer())
                        .push(http::BoxResponse::layer()),
                )
                .check_new_service::<T, http::Request<_>>()
                .push(NewAccessLog::layer())
                .instrument(|_: &T| debug_span!("http"))
                .push(http::NewServeHttp::layer(h2_settings, rt.drain.clone()))
                .push_on_service(svc::BoxService::layer())
                .push(svc::ArcNewService::layer())
        })
    }
}

// === impl ServerRescue ===

impl ServerRescue {
    /// Synthesizes responses for HTTP requests that encounter proxy errors.
    pub fn layer<N>(
    ) -> impl svc::layer::Layer<N, Service = errors::NewRespondService<Self, Self, N>> + Clone {
        errors::respond::layer(Self)
    }
}

impl<T> ExtractParam<Self, T> for ServerRescue {
    #[inline]
    fn extract_param(&self, _: &T) -> Self {
        *self
    }
}

impl<T: Param<tls::ConditionalServerTls>> ExtractParam<errors::respond::EmitHeaders, T>
    for ServerRescue
{
    #[inline]
    fn extract_param(&self, t: &T) -> errors::respond::EmitHeaders {
        // Only emit informational headers to meshed peers.
        let emit = t
            .param()
            .value()
            .map(|tls| match tls {
                tls::ServerTls::Established { client_id, .. } => client_id.is_some(),
                _ => false,
            })
            .unwrap_or(false);
        errors::respond::EmitHeaders(emit)
    }
}

impl errors::HttpRescue<Error> for ServerRescue {
    fn rescue(&self, error: Error) -> Result<errors::SyntheticHttpResponse> {
        if errors::is_caused_by::<policy::HttpRouteNotFound>(&*error) {
            return Ok(errors::SyntheticHttpResponse::not_found(error));
        }

        if errors::is_caused_by::<policy::HttpRouteUnauthorized>(&*error) {
            return Ok(errors::SyntheticHttpResponse::permission_denied(error));
        }

        if errors::is_caused_by::<policy::HttpRouteInvalidRedirect>(&*error) {
            tracing::warn!(%error);
            return Ok(errors::SyntheticHttpResponse::unexpected_error());
        }
        if let Some(policy::HttpRouteRedirect { status, location }) =
            errors::cause_ref::<policy::HttpRouteRedirect>(&*error)
        {
            return Ok(errors::SyntheticHttpResponse::redirect(*status, location));
        }
        if errors::is_caused_by::<policy::HttpInvalidPolicy>(&*error) {
            return Ok(errors::SyntheticHttpResponse::internal_error(
                error.to_string(),
            ));
        }

        if errors::is_caused_by::<crate::GatewayDomainInvalid>(&*error) {
            return Ok(errors::SyntheticHttpResponse::not_found(error));
        }
        if errors::is_caused_by::<crate::GatewayIdentityRequired>(&*error) {
            return Ok(errors::SyntheticHttpResponse::unauthenticated(error));
        }
        if errors::is_caused_by::<crate::GatewayLoop>(&*error) {
            return Ok(errors::SyntheticHttpResponse::loop_detected(error));
        }
        if errors::is_caused_by::<errors::FailFastError>(&*error) {
            return Ok(errors::SyntheticHttpResponse::gateway_timeout(error));
        }
        if errors::is_caused_by::<errors::LoadShedError>(&*error) {
            return Ok(errors::SyntheticHttpResponse::unavailable(error));
        }

        if errors::is_caused_by::<errors::H2Error>(&*error) {
            return Err(error);
        }

        tracing::warn!(error, "Unexpected error");
        Ok(errors::SyntheticHttpResponse::unexpected_error())
    }
}
