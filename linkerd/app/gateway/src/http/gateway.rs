use futures::{future, TryFutureExt};
use linkerd_app_core::{
    identity as id,
    proxy::http,
    svc::{self, layer},
    tls, Error,
};
use linkerd_app_inbound::{GatewayAddr, GatewayLoop};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

/// A `NewService` that wraps inner services with [`HttpGateway`].
#[derive(Clone, Debug)]
pub(crate) struct NewHttpGateway<N> {
    inner: N,
    local_id: id::Id,
}

/// A `Service` middleware that fails requests that would loop. It reads and
/// updates the `forwarded` header to detect loops.
#[derive(Clone, Debug)]
pub(crate) struct HttpGateway<S> {
    inner: S,
    host: String,
    client_id: tls::ClientId,
    local_id: id::Id,
}

type ResponseFuture<T> = Pin<Box<dyn Future<Output = Result<T, Error>> + Send + 'static>>;

// === impl NewHttpGateway ===

impl<N> NewHttpGateway<N> {
    pub fn new(inner: N, local_id: id::Id) -> Self {
        Self { inner, local_id }
    }

    pub fn layer(local_id: id::Id) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(inner, local_id.clone()))
    }
}

impl<T, N> svc::NewService<T> for NewHttpGateway<N>
where
    T: svc::Param<GatewayAddr>,
    T: svc::Param<tls::ClientId>,
    N: svc::NewService<T> + Clone + Send + 'static,
{
    type Service = HttpGateway<N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let GatewayAddr(addr) = target.param();
        HttpGateway {
            host: addr.as_http_authority().to_string(),
            client_id: target.param(),
            local_id: self.local_id.clone(),
            inner: self.inner.new_service(target),
        }
    }
}

// === impl HttpGateway ===

impl<B, S> tower::Service<http::Request<B>> for HttpGateway<S>
where
    B: http::Body + 'static,
    S: tower::Service<http::Request<B>, Response = http::Response<http::BoxBody>>,
    S::Error: Into<Error> + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = Error;
    type Future = ResponseFuture<S::Response>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // If the client ID is the same as the gateway's, then we're in a loop.
        if *self.client_id == self.local_id {
            return Poll::Ready(Err(GatewayLoop.into()));
        }

        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, mut request: http::Request<B>) -> Self::Future {
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
                if by == self.local_id.to_str() {
                    return Box::pin(future::err(GatewayLoop.into()));
                }
            }
        }

        // Determine the value of the forwarded header using the Client
        // ID from the requests's extensions.
        let fwd = format!(
            "by={};for={};host={};proto=https",
            self.local_id, self.client_id, self.host
        );
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
                http::header::HeaderValue::from_str(&self.host)
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
