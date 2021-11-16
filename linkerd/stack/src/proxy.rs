use futures::{future, TryFutureExt};
use linkerd_error::Error;
use std::future::Future;
use std::task::{Context, Poll};

/// A middleware type that cannot exert backpressure.
///
/// Typically used to modify requests or responses.
pub trait Proxy<Req, S: tower::Service<Self::Request>> {
    /// The type of request sent to the inner `S`-typed service.
    type Request;

    /// The type of response returned to callers.
    type Response;

    /// The error type returned to callers.
    type Error: Into<Error>;

    /// The Future type returned to callers.
    type Future: Future<Output = Result<Self::Response, Self::Error>>;

    /// Usually invokes `S::call`, potentially modifying requests or responses.
    fn proxy(&self, inner: &mut S, req: Req) -> Self::Future;

    /// Wraps an `S` typed service with the proxy.
    fn wrap_service(self, inner: S) -> ProxyService<Self, S>
    where
        Self: Sized,
        S: tower::Service<Self::Request>,
    {
        ProxyService::new(self, inner)
    }
}

/// Wraps an `S`-typed `Service` with a `P`-typed `Proxy`.
#[derive(Clone, Debug)]
pub struct ProxyService<P, S> {
    proxy: P,
    service: S,
}

// === impl Proxy ===

/// The identity Proxy.
impl<Req, S> Proxy<Req, S> for ()
where
    S: tower::Service<Req>,
    S::Error: Into<Error>,
{
    type Request = Req;
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn proxy(&self, inner: &mut S, req: Req) -> Self::Future {
        inner.call(req)
    }
}

// === impl ProxyService ===

impl<P, S> ProxyService<P, S> {
    pub fn new(proxy: P, service: S) -> Self {
        Self { proxy, service }
    }

    pub fn into_parts(self) -> (P, S) {
        (self.proxy, self.service)
    }
}

impl<Req, P, S> tower::Service<Req> for ProxyService<P, S>
where
    P: Proxy<Req, S>,
    S: tower::Service<P::Request>,
    S::Error: Into<Error>,
{
    type Response = P::Response;
    type Error = Error;
    type Future = future::MapErr<P::Future, fn(P::Error) -> Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        self.proxy.proxy(&mut self.service, req).map_err(Into::into)
    }
}
