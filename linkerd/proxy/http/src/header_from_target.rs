use futures::{try_ready, Future, Poll};
use http;
use http::header::{HeaderValue, IntoHeaderName};
use linkerd2_stack::NewService;

pub trait ExtractHeader<T> {
    fn extract(&self, target: &T) -> Option<HeaderValue>;
}

/// Wraps HTTP `Service`s  so that a displayable `T` is cloned into each request's
/// extensions.
#[derive(Debug, Clone)]
pub struct Layer<H, E> {
    header: H,
    extractor: E,
}

/// Wraps an HTTP `Service` so that the Stack's `T -typed target` is cloned into
/// each request's headers.
#[derive(Clone, Debug)]
pub struct MakeSvc<H, E, M> {
    header: H,
    extractor: E,
    inner: M,
}

#[derive(Clone, Debug)]
pub struct Service<H, S> {
    header: H,
    value: Option<HeaderValue>,
    inner: S,
}

// === impl Layer ===

pub fn layer<H, E>(header: H, extractor: E) -> Layer<H, E>
where
    H: IntoHeaderName + Clone,
    E: Clone,
{
    Layer { header, extractor }
}

impl<H, E, M> tower::layer::Layer<M> for Layer<H, E>
where
    H: IntoHeaderName + Clone,
    E: Clone,
{
    type Service = MakeSvc<H, E, M>;

    fn layer(&self, inner: M) -> Self::Service {
        MakeSvc {
            header: self.header.clone(),
            extractor: self.extractor.clone(),
            inner,
        }
    }
}

// === impl MakeSvc ===

impl<H, E, T, M> NewService<T> for MakeSvc<H, E, M>
where
    H: IntoHeaderName + Clone,
    T: Clone + Send + Sync + 'static,
    M: NewService<T>,
    E: ExtractHeader<T>,
{
    type Service = Service<H, M::Service>;

    fn new_service(&self, t: T) -> Self::Service {
        let header = self.header.clone();
        let value = self.extractor.extract(&t);
        let inner = self.inner.new_service(t);
        Service {
            header,
            inner,
            value,
        }
    }
}

impl<H, E, T, M> tower::Service<T> for MakeSvc<H, E, M>
where
    H: IntoHeaderName + Clone,
    T: Clone + Send + Sync + 'static,
    M: tower::Service<T>,
    E: ExtractHeader<T>,
    E: Clone,
{
    type Response = Service<H, M::Response>;
    type Error = M::Error;
    type Future = MakeSvc<(H, Option<HeaderValue>), E, M::Future>;

    fn poll_ready(&mut self) -> Poll<(), M::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, t: T) -> Self::Future {
        let header = self.header.clone();
        let extractor = self.extractor.clone();
        let value = extractor.extract(&t);
        let inner = self.inner.call(t);

        MakeSvc {
            header: (header, value),
            extractor,
            inner,
        }
    }
}

impl<H, E, F> Future for MakeSvc<(H, Option<HeaderValue>), E, F>
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
        if let Some(header_value) = self.value.clone() {
            req.headers_mut().insert(self.header.clone(), header_value);
        }
        self.inner.call(req)
    }
}

// === impl ExtractHeader ===

impl<T, F> ExtractHeader<T> for F
where
    F: Fn(&T) -> Option<HeaderValue>,
{
    fn extract(&self, _target: &T) -> Option<HeaderValue> {
        Option::None
    }
}
