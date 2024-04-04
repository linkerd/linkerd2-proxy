use super::set_identity_header::NewSetIdentityHeader;
use crate::{policy, Inbound};
pub use linkerd_app_core::proxy::http::Version;
use linkerd_app_core::{
    config::ProxyConfig,
    errors, http_tracing, io,
    metrics::ServerLabel,
    proxy::http,
    svc::{self, ExtractParam, Param},
    tls,
    transport::{ClientAddr, OrigDstAddr, Remote},
    Error, Result,
};
use linkerd_http_access_log::NewAccessLog;

#[derive(Copy, Clone, Debug)]
struct ServerRescue;

#[derive(Debug, thiserror::Error)]
#[error("client {client}: server: {dst}: {source}")]
struct ServerError {
    client: Remote<ClientAddr>,
    dst: OrigDstAddr,
    #[source]
    source: Error,
}

impl<H> Inbound<H> {
    /// Prepares HTTP requests for inbound processing. Fails requests when the
    /// `HSvc`-typed inner service is not ready.
    pub fn push_http_server<T, HSvc>(self) -> Inbound<svc::ArcNewCloneHttp<T>>
    where
        // Connection target.
        T: Param<Version>
            + Param<tls::ConditionalServerTls>
            + Param<ServerLabel>
            + Param<OrigDstAddr>
            + Param<Remote<ClientAddr>>,
        T: Clone + Send + Sync + Unpin + 'static,
        // Inner HTTP stack.
        H: svc::NewService<T, Service = HSvc> + Clone + Send + Sync + Unpin + 'static,
        HSvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>
            + Clone
            + Send
            + Sync
            + Unpin
            + 'static,
        HSvc::Error: Into<Error>,
        HSvc::Future: Send,
    {
        self.map_stack(|config, rt, http| {
            let ProxyConfig {
                max_in_flight_requests,
                ..
            } = config.proxy;

            http.check_new_service::<T, http::Request<_>>()
                .push(NewSetIdentityHeader::layer(()))
                // Limit the number of in-flight inbound requests.
                //
                // TODO(ver) This concurrency limit applies only to
                // requests that do not yet have responses, but ignores
                // streaming bodies. We should change this to an
                // HTTP-specific imlementation that tracks request and
                // response bodies.
                .push_on_service(svc::ConcurrencyLimitLayer::new(max_in_flight_requests))
                // Shed load by failing requests when the concurrency
                // limit is reached.
                .push_on_service(svc::LoadShed::layer())
                .push(svc::NewMapErr::layer_from_target::<ServerError, _>())
                .push_on_service(svc::MapErr::layer_boxed())
                .push(rt.metrics.http_errors.to_layer())
                .push(ServerRescue::layer())
                .push_on_service(http_tracing::server(
                    rt.span_sink.clone(),
                    super::trace_labels(),
                ))
                .push_on_service(http::BoxResponse::layer())
                .push(NewAccessLog::layer())
                .arc_new_clone_http()
        })
    }

    /// Uses the inner stack to serve HTTP requests for the given server-side
    /// socket.
    pub fn push_http_tcp_server<T, I, HSvc>(self) -> Inbound<svc::ArcNewTcp<T, I>>
    where
        // Connection target.
        T: Param<http::server::DefaultAuthority>,
        T: Param<tls::ConditionalServerTls>,
        T: Param<Version>,
        T: Clone + Send + Unpin + 'static,
        // Server-side socket.
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + Send + Unpin + 'static,
        // Inner HTTP stack.
        H: svc::NewService<T, Service = HSvc> + Clone + Send + Sync + Unpin + 'static,
        HSvc: svc::Service<
                http::Request<http::BoxBody>,
                Response = http::Response<http::BoxBody>,
                Error = Error,
            > + Clone
            + Send
            + Unpin
            + 'static,
        HSvc::Future: Send,
    {
        self.map_stack(|config, rt, http| {
            let h2 = config.proxy.server.h2_settings;
            let drain = rt.drain.clone();

            http.check_new_service::<T, http::Request<http::BoxBody>>()
                .unlift_new()
                .check_new_new_service::<T, http::ClientHandle, http::Request<_>>()
                .push(http::NewServeHttp::layer(drain, move |t: &T| {
                    let default_authority = t.param();
                    http::ServerParams {
                        version: t.param(),
                        h2,
                        default_authority,
                        // TODO(ver) this should be conditional on whether the
                        // client is meshed, but our integration tests currently
                        // require that it always be set.
                        supports_orig_proto_downgrades: true,
                    }
                }))
                .check_new_service::<T, I>()
                .arc_new_tcp()
        })
    }
}

impl<T> From<(&T, Error)> for ServerError
where
    T: Param<OrigDstAddr>,
    T: Param<Remote<ClientAddr>>,
{
    fn from((t, source): (&T, Error)) -> Self {
        Self {
            client: t.param(),
            dst: t.param(),
            source,
        }
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
