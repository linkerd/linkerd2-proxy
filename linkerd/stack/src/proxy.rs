use linkerd_error::Error;
use std::future::Future;

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

    #[inline]
    fn proxy(&self, inner: &mut S, req: Req) -> Self::Future {
        inner.call(req)
    }
}
