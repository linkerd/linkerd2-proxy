use http::header::{HeaderValue, IntoHeaderName};
use linkerd_stack::{layer, NewService};
use std::task::{Context, Poll};

/// Wraps an HTTP `Service` so that the Stack's `T -typed target` is cloned into
/// each request's headers.
#[derive(Clone, Debug)]
pub struct NewHeaderFromTarget<H, M> {
    header: H,
    inner: M,
}

#[derive(Clone, Debug)]
pub struct HeaderFromTarget<H, S> {
    header: H,
    value: HeaderValue,
    inner: S,
}

// === impl NewHeaderFromTarget ===

impl<H: Clone, N> NewHeaderFromTarget<H, N> {
    pub fn layer(header: H) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self {
            inner,
            header: header.clone(),
        })
    }
}

impl<H, T, N> NewService<T> for NewHeaderFromTarget<H, N>
where
    H: IntoHeaderName + Clone,
    T: Clone + Send + Sync + 'static,
    HeaderValue: for<'t> From<&'t T>,
    N: NewService<T>,
{
    type Service = HeaderFromTarget<H, N::Service>;

    fn new_service(&mut self, t: T) -> Self::Service {
        HeaderFromTarget {
            value: (&t).into(),
            inner: self.inner.new_service(t),
            header: self.header.clone(),
        }
    }
}

// === impl HeaderFromTarget ===

impl<H, S, B> tower::Service<http::Request<B>> for HeaderFromTarget<H, S>
where
    H: IntoHeaderName + Clone,
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
            .insert(self.header.clone(), self.value.clone());
        self.inner.call(req)
    }
}
