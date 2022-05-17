use super::set_identity_header::NewSetIdentityHeader;
use crate::Inbound;
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
                dispatch_timeout,
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
                        // Limit the number of in-flight requests. When the proxy is
                        // at capacity, go into failfast after a dispatch timeout.
                        // Note that the inner service _always_ returns ready (due
                        // to `NewRouter`) and the concurrency limit need not be
                        // driven outside of the request path, so there's no need
                        // for SpawnReady
                        .push(svc::ConcurrencyLimitLayer::new(max_in_flight_requests))
                        .push(svc::FailFast::layer("HTTP Server", dispatch_timeout)),
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
                .instrument(|t: &T| debug_span!("http", v = %Param::<Version>::param(t)))
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
        if let Some(cause) = errors::cause_ref::<crate::policy::DeniedUnauthorized>(&*error) {
            return Ok(errors::SyntheticHttpResponse::permission_denied(cause));
        }
        if let Some(cause) = errors::cause_ref::<crate::GatewayDomainInvalid>(&*error) {
            return Ok(errors::SyntheticHttpResponse::not_found(cause));
        }
        if let Some(cause) = errors::cause_ref::<crate::GatewayIdentityRequired>(&*error) {
            return Ok(errors::SyntheticHttpResponse::unauthenticated(cause));
        }
        if let Some(cause) = errors::cause_ref::<crate::GatewayLoop>(&*error) {
            return Ok(errors::SyntheticHttpResponse::loop_detected(cause));
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
