use crate::HeaderPair;
use http::header::{HeaderName, HeaderValue};
use linkerd_stack::{layer, ExtractParam, NewService, Service};
use std::task::{Context, Poll};

/// Wraps an HTTP `Service` so that the Stack's `T -typed target` is cloned into
/// each request's headers.
#[derive(Clone, Debug)]
pub struct NewHeaderFromTarget<H, X, N> {
    inner: N,
    extract: X,
    _marker: std::marker::PhantomData<fn() -> H>,
}

#[derive(Clone, Debug)]
pub struct HeaderFromTarget<S> {
    name: HeaderName,
    value: HeaderValue,
    inner: S,
}

// === impl NewHeaderFromTarget ===

impl<H, X: Clone, N> NewHeaderFromTarget<H, X, N> {
    pub fn layer_via(extract: X) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self {
            inner,
            extract: extract.clone(),
            _marker: std::marker::PhantomData,
        })
    }
}

impl<H, N> NewHeaderFromTarget<H, (), N> {
    pub fn layer() -> impl layer::Layer<N, Service = Self> + Clone {
        Self::layer_via(())
    }
}

impl<H, T, X, N> NewService<T> for NewHeaderFromTarget<H, X, N>
where
    H: Into<HeaderPair>,
    X: ExtractParam<H, T>,
    N: NewService<T>,
{
    type Service = HeaderFromTarget<N::Service>;

    fn new_service(&self, t: T) -> Self::Service {
        let HeaderPair(name, value) = self.extract.extract_param(&t).into();
        let inner = self.inner.new_service(t);
        HeaderFromTarget { name, value, inner }
    }
}

// === impl HeaderFromTarget ===

impl<S, B> Service<http::Request<B>> for HeaderFromTarget<S>
where
    S: Service<http::Request<B>>,
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
