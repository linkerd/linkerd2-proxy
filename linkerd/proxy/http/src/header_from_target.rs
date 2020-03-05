use futures::{try_ready, Future, Poll};
use http;
use http::header::{HeaderValue, IntoHeaderName};
use linkerd2_stack::NewService;

/// Wraps HTTP `Service`s  so that a displayable `T` is cloned into each request's
/// extensions.
#[derive(Debug, Clone)]
pub struct Layer<H> {
    header: H,
}

/// Wraps an HTTP `Service` so that the Stack's `T -typed target` is cloned into
/// each request's headers.
#[derive(Clone, Debug)]
pub struct MakeSvc<H, M> {
    header: H,
    inner: M,
}

#[derive(Clone, Debug)]
pub struct Service<H, S> {
    header: H,
    value: HeaderValue,
    inner: S,
}

// === impl Layer ===

pub fn layer<H>(header: H) -> Layer<H>
where
    H: IntoHeaderName + Clone,
{
    Layer { header }
}

impl<H, M> tower::layer::Layer<M> for Layer<H>
where
    H: IntoHeaderName + Clone,
{
    type Service = MakeSvc<H, M>;

    fn layer(&self, inner: M) -> Self::Service {
        MakeSvc {
            header: self.header.clone(),
            inner,
        }
    }
}

// === impl MakeSvc ===

impl<H, T, M> NewService<T> for MakeSvc<H, M>
where
    H: IntoHeaderName + Clone,
    T: Clone + Send + Sync + 'static,
    HeaderValue: for<'t> From<&'t T>,
    M: NewService<T>,
{
    type Service = Service<H, M::Service>;

    fn new_service(&self, t: T) -> Self::Service {
        let header = self.header.clone();
        let value = (&t).into();
        let inner = self.inner.new_service(t);
        Service {
            header,
            inner,
            value,
        }
    }
}

impl<H, T, M> tower::Service<T> for MakeSvc<H, M>
where
    H: IntoHeaderName + Clone,
    T: Clone + Send + Sync + 'static,
    HeaderValue: for<'t> From<&'t T>,
    M: tower::Service<T>,
{
    type Response = Service<H, M::Response>;
    type Error = M::Error;
    type Future = MakeSvc<(H, HeaderValue), M::Future>;

    fn poll_ready(&mut self) -> Poll<(), M::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, t: T) -> Self::Future {
        let header = self.header.clone();
        let value = (&t).into();
        let inner = self.inner.call(t);

        MakeSvc {
            header: (header, value),
            inner,
        }
    }
}

impl<H, F> Future for MakeSvc<(H, HeaderValue), F>
where
    H: Clone,
    F: Future,
{
    type Item = Service<H, F::Item>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());
        let (header, value) = self.header.clone();
        Ok(Service {
            header,
            inner,
            value,
        }
        .into())
    }
}

// === impl Service ===

impl<H, S, B> tower::Service<http::Request<B>> for Service<H, S>
where
    H: IntoHeaderName + Clone,
    S: tower::Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        req.headers_mut()
            .insert(self.header.clone(), self.value.clone());
        self.inner.call(req)
    }
}
