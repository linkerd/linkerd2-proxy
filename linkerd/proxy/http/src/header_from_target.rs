use crate::HeaderPair;
use http::header::{HeaderName, HeaderValue};
use linkerd_stack::{layer, NewService, Param};
use std::task::{Context, Poll};

/// Wraps an HTTP `Service` so that the Stack's `T -typed target` is cloned into
/// each request's headers.
#[derive(Clone, Debug)]
pub struct NewHeaderFromTarget<H, N> {
    inner: N,
    _marker: std::marker::PhantomData<fn() -> H>,
}

#[derive(Clone, Debug)]
pub struct HeaderFromTarget<S> {
    name: HeaderName,
    value: HeaderValue,
    inner: S,
}

// === impl NewHeaderFromTarget ===

impl<H, N> NewHeaderFromTarget<H, N> {
    pub fn layer() -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self {
            inner,
            _marker: std::marker::PhantomData,
        })
    }
}

impl<H, T, N> NewService<T> for NewHeaderFromTarget<H, N>
where
    H: Into<HeaderPair>,
    T: Param<H>,
    N: NewService<T>,
{
    type Service = HeaderFromTarget<N::Service>;

    fn new_service(&self, t: T) -> Self::Service {
        let HeaderPair(name, value) = t.param().into();
        let inner = self.inner.new_service(t);
        HeaderFromTarget { name, value, inner }
    }
}

// === impl HeaderFromTarget ===

impl<S, B> tower::Service<http::Request<B>> for HeaderFromTarget<S>
where
    S: tower::Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    #[inline]
    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        req.headers_mut()
            .insert(self.name.clone(), self.value.clone());
        self.inner.call(req)
    }
}
