//! A stack that sends requests to an HTTP endpoint.

use super::{NewRequireIdentity, NewStripProxyError, ProxyConnectionClose};
use crate::{tcp::tagged_transport, Outbound};
use linkerd_app_core::{
    classify, config, errors, http_tracing, metrics,
    proxy::{http, tap},
    svc::{self, ExtractParam},
    tls,
    transport::{self, Remote, ServerAddr},
    transport_header::SessionProtocol,
    Error, Result, CANONICAL_DST_HEADER,
};

#[cfg(test)]
mod tests;

#[derive(Clone, Debug)]
pub struct Connect<T> {
    version: http::Version,
    inner: T,
}

#[derive(Debug, thiserror::Error)]
#[error("endpoint {addr}: {source}")]
pub struct EndpointError {
    addr: Remote<ServerAddr>,
    #[source]
    source: Error,
}

#[derive(Copy, Clone, Debug)]
struct ClientRescue {
    emit_headers: bool,
}

impl<C> Outbound<C> {
    pub fn push_http_endpoint<T, B>(self) -> Outbound<svc::ArcNewHttp<T, B>>
    where
        // Http endpoint target.
        T: svc::Param<http::client::Settings>,
        T: svc::Param<Remote<ServerAddr>>,
        T: svc::Param<Option<http::AuthorityOverride>>,
        T: svc::Param<metrics::EndpointLabels>,
        T: svc::Param<tls::ConditionalClientTls>,
        T: tap::Inspect,
        T: Clone + Send + Sync + 'static,
        // Http endpoint body.
        B: http::HttpBody<Error = Error> + std::fmt::Debug + Default + Send + 'static,
        B::Data: Send + 'static,
        // TCP endpoint stack.
        C: svc::MakeConnection<Connect<T>> + Clone + Send + Sync + Unpin + 'static,
        C::Connection: Send + Unpin,
        C::Metadata: Send + Unpin,
        C::Future: Send + Unpin + 'static,
    {
        self.map_stack(|config, rt, inner| {
            let config::ConnectConfig {
                h1_settings,
                h2_settings,
                backoff,
                ..
            } = config.proxy.connect;

            // Initiates an HTTP client on the underlying transport. Prior-knowledge HTTP/2
            // is typically used (i.e. when communicating with other proxies); though
            // HTTP/1.x fallback is supported as needed.
            svc::stack(inner.into_inner().into_service())
                .check_service::<Connect<T>>()
                .push_map_target(|(version, inner)| Connect { version, inner })
                .push(http::client::layer(h1_settings, h2_settings))
                .push_on_service(svc::MapErr::layer_boxed())
                .check_service::<T>()
                .into_new_service()
                // Drive the connection to completion regardless of whether the reconnect is being
                // actively polled.
                .push_on_service(svc::layer::mk(svc::SpawnReady::new))
                .push_new_reconnect(backoff)
                // Set the TLS status on responses so that the stack can detect whether the request
                // was sent over a meshed connection.
                .push_http_response_insert_target::<tls::ConditionalClientTls>()
                .push(svc::NewMapErr::layer_from_target::<EndpointError, _>())
                // If the outbound proxy is not configured to emit headers, then strip the
                // `l5d-proxy-errors` header if set by the peer.
                .push(NewStripProxyError::layer(config.emit_headers))
                // Tear down server connections when a peer proxy generates an error.
                // TODO(ver) this should only be honored when forwarding and not when the connection
                // is part of a balancer.
                .push(ProxyConnectionClose::layer())
                // Handle connection-level errors eagerly so that we can report 5XX failures in tap
                // and metrics. HTTP error metrics are not incremented here so that errors are not
                // double-counted--i.e., endpoint metrics track these responses and error metrics
                // track proxy errors that occur higher in the stack.
                .push(ClientRescue::layer(config.emit_headers))
                .push(tap::NewTapHttp::layer(rt.tap.clone()))
                .push(
                    rt.metrics
                        .proxy
                        .http_endpoint
                        .to_layer::<classify::Response, _, _>(),
                )
                .push_on_service(http_tracing::client(
                    rt.span_sink.clone(),
                    crate::trace_labels(),
                ))
                .push(NewRequireIdentity::layer())
                .push(http::NewOverrideAuthority::layer(vec![
                    "host",
                    CANONICAL_DST_HEADER,
                ]))
                .push_on_service(
                    svc::layers()
                        .push(http::BoxResponse::layer())
                        .push(svc::BoxService::layer()),
                )
                .push(svc::ArcNewService::layer())
        })
    }
}

// === impl ClientRescue ===

impl ClientRescue {
    /// Synthesizes responses for HTTP requests that encounter proxy errors.
    pub fn layer<N>(
        emit_headers: bool,
    ) -> impl svc::layer::Layer<N, Service = errors::NewRespondService<Self, Self, N>> + Clone {
        errors::respond::layer(Self { emit_headers })
    }
}

impl<T> ExtractParam<Self, T> for ClientRescue {
    #[inline]
    fn extract_param(&self, _: &T) -> Self {
        *self
    }
}

impl<T> ExtractParam<errors::respond::EmitHeaders, T> for ClientRescue {
    #[inline]
    fn extract_param(&self, _: &T) -> errors::respond::EmitHeaders {
        // Always emit informational headers on responses to an application.
        errors::respond::EmitHeaders(self.emit_headers)
    }
}

impl errors::HttpRescue<Error> for ClientRescue {
    fn rescue(&self, error: Error) -> Result<errors::SyntheticHttpResponse> {
        if errors::is_caused_by::<http::orig_proto::DowngradedH2Error>(&*error) {
            return Ok(errors::SyntheticHttpResponse::bad_gateway(error));
        }
        if errors::is_caused_by::<std::io::Error>(&*error) {
            return Ok(errors::SyntheticHttpResponse::bad_gateway(error));
        }
        if errors::is_caused_by::<errors::ConnectTimeout>(&*error) {
            return Ok(errors::SyntheticHttpResponse::gateway_timeout(error));
        }

        Err(error)
    }
}

// === impl Connect ===

impl<T> svc::Param<Option<SessionProtocol>> for Connect<T> {
    #[inline]
    fn param(&self) -> Option<SessionProtocol> {
        match self.version {
            http::Version::Http1 => Some(SessionProtocol::Http1),
            http::Version::H2 => Some(SessionProtocol::Http2),
        }
    }
}

impl<T: svc::Param<Remote<ServerAddr>>> svc::Param<Remote<ServerAddr>> for Connect<T> {
    #[inline]
    fn param(&self) -> Remote<ServerAddr> {
        self.inner.param()
    }
}

impl<T: svc::Param<tls::ConditionalClientTls>> svc::Param<tls::ConditionalClientTls>
    for Connect<T>
{
    #[inline]
    fn param(&self) -> tls::ConditionalClientTls {
        self.inner.param()
    }
}

impl<T: svc::Param<Option<tagged_transport::PortOverride>>>
    svc::Param<Option<tagged_transport::PortOverride>> for Connect<T>
{
    #[inline]
    fn param(&self) -> Option<tagged_transport::PortOverride> {
        self.inner.param()
    }
}

impl<T: svc::Param<Option<http::AuthorityOverride>>> svc::Param<Option<http::AuthorityOverride>>
    for Connect<T>
{
    #[inline]
    fn param(&self) -> Option<http::AuthorityOverride> {
        self.inner.param()
    }
}

impl<T: svc::Param<transport::labels::Key>> svc::Param<transport::labels::Key> for Connect<T> {
    #[inline]
    fn param(&self) -> transport::labels::Key {
        self.inner.param()
    }
}

// === impl EndpointError ===

impl<T> From<(&T, Error)> for EndpointError
where
    T: svc::Param<Remote<ServerAddr>>,
{
    fn from((target, source): (&T, Error)) -> Self {
        Self {
            addr: target.param(),
            source,
        }
    }
}
