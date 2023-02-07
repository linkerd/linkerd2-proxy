use crate::HttpOutbound;
use futures::{future, TryFutureExt};
use linkerd_app_core::{
    proxy::http,
    svc::{self, layer},
    tls, Error,
};
use linkerd_app_inbound::{GatewayAddr, GatewayIdentityRequired, GatewayLoop};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

#[derive(Clone, Debug)]
pub(crate) struct NewHttpGateway<N> {
    inner: N,
    local_id: tls::LocalId,
}

#[derive(Clone, Debug)]
pub(crate) struct HttpGateway<N> {
    inner: N,
    host: String,
    client_id: tls::ClientId,
    local_id: tls::LocalId,
}

type ResponseFuture<T> = Pin<Box<dyn Future<Output = Result<T, Error>> + Send + 'static>>;

// === impl NewHttpGateway ===

impl<N> NewHttpGateway<N> {
    pub fn new(inner: N, local_id: tls::LocalId) -> Self {
        Self { inner, local_id }
    }

    pub fn layer(local_id: tls::LocalId) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(inner, local_id.clone()))
    }
}

impl<T, N> svc::NewService<HttpOutbound<T>> for NewHttpGateway<N>
where
    T: svc::Param<GatewayAddr>,
    T: svc::Param<tls::ClientId>,
    N: svc::NewService<HttpOutbound<T>> + Clone + Send + 'static,
{
    type Service = HttpGateway<N::Service>;

    fn new_service(&self, target: HttpOutbound<T>) -> Self::Service {
        let host = {
            let GatewayAddr(addr) = (*target).param();
            addr.as_http_authority().to_string()
        };
        HttpGateway {
            host,
            client_id: (*target).param(),
            local_id: self.local_id.clone(),
            inner: self.inner.new_service(target),
        }
    }
}

// === impl HttpGateway ===

impl<B, S> tower::Service<http::Request<B>> for HttpGateway<S>
where
    B: http::HttpBody + 'static,
    S: tower::Service<http::Request<B>, Response = http::Response<http::BoxBody>>,
    S::Error: Into<Error> + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = Error;
    type Future = ResponseFuture<S::Response>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, mut request: http::Request<B>) -> Self::Future {
        let tls::LocalId(ref local_id) = self.local_id;

        // Check forwarded headers to see if this request has already
        // transited through this gateway.
        for forwarded in request
            .headers()
            .get_all(http::header::FORWARDED)
            .into_iter()
            .filter_map(|h| h.to_str().ok())
        {
            if let Some(by) = fwd_by(forwarded) {
                tracing::info!(%forwarded);
                if by == local_id.as_str() {
                    return Box::pin(future::err(GatewayLoop.into()));
                }
            }
        }

        // Determine the value of the forwarded header using the Client
        // ID from the requests's extensions.
        let fwd = match request.extensions_mut().remove::<tls::ClientId>() {
            Some(client_id) => format!(
                "by={};for={};host={};proto=https",
                local_id, client_id, self.host
            ),
            None => {
                tracing::warn!("Request missing ClientId extension");
                return Box::pin(future::err(GatewayIdentityRequired.into()));
            }
        };
        request.headers_mut().append(
            http::header::FORWARDED,
            http::header::HeaderValue::from_str(&fwd)
                .expect("Forwarded header value must be valid"),
        );

        // If we're forwarding HTTP/1 requests, the old `Host` header
        // was stripped on the peer's outbound proxy. But the request
        // should have an updated `Host` header now that it's being
        // routed in the cluster.
        if let ::http::Version::HTTP_11 | ::http::Version::HTTP_10 = request.version() {
            request.headers_mut().insert(
                http::header::HOST,
                http::header::HeaderValue::from_str(&*self.host)
                    .expect("Host header value must be valid"),
            );
        }

        tracing::trace!("Passing request to outbound");
        Box::pin(self.inner.call(request).map_err(Into::into))
    }
}

fn fwd_by(fwd: &str) -> Option<&str> {
    for kv in fwd.split(';') {
        let mut kv = kv.split('=');
        if let Some("by") = kv.next() {
            return kv.next();
        }
    }
    None
}
