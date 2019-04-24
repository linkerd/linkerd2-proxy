use futures::{Future, Poll};
use http;

use svc;

/// Wraps HTTP `Service` `Stack<T>`s so that `T` is cloned into each request's
/// extensions.
#[derive(Debug, Clone)]
pub struct Layer;

/// Wraps an HTTP `Service` so that the Stack's `T -typed target` is cloned into
/// each request's extensions.
#[derive(Clone, Debug)]
pub struct Stack<M>(M);

pub struct MakeFuture<F, T> {
    inner: F,
    target: T,
}

#[derive(Clone, Debug)]
pub struct Service<T, S> {
    target: T,
    inner: S,
}

// === impl Layer ===

pub fn layer() -> Layer {
    Layer
}

impl<M> svc::Layer<M> for Layer {
    type Service = Stack<M>;

    fn layer(&self, next: M) -> Self::Service {
        Stack(next)
    }
}

// === impl Stack ===

impl<T, M> svc::Service<T> for Stack<M>
where
    T: Clone + Send + Sync + 'static,
    M: svc::Service<T>,
{
    type Response = Service<T, M::Response>;
    type Error = M::Error;
    type Future = MakeFuture<M::Future, T>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.0.poll_ready()
    }

    fn call(&mut self, t: T) -> Self::Future {
        let target = t.clone();
        let inner = self.0.call(t);
        MakeFuture { inner, target }
    }
}

// === impl MakeFuture ===

impl<F, T> Future for MakeFuture<F, T>
where
    F: Future,
    T: Clone,
{
    type Item = Service<T, F::Item>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());
        Ok(Service {
            inner,
            target: self.target.clone(),
        }
        .into())
    }
}

// === impl Service ===

impl<T, S, B> svc::Service<http::Request<B>> for Service<T, S>
where
    T: Clone + Send + Sync + 'static,
    S: svc::Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        req.extensions_mut().insert(self.target.clone());
        self.inner.call(req)
    }
}
