use futures::Poll;
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

#[derive(Clone, Debug)]
pub struct Service<T, S> {
    target: T,
    inner: S,
}

// === impl Layer ===

pub fn layer() -> Layer {
    Layer
}

impl<T, M> svc::Layer<T, T, M> for Layer
where
    T: Clone + Send + Sync + 'static,
    M: svc::Stack<T>,
{
    type Value = <Stack<M> as svc::Stack<T>>::Value;
    type Error = <Stack<M> as svc::Stack<T>>::Error;
    type Stack = Stack<M>;

    fn bind(&self, next: M) -> Self::Stack {
        Stack(next)
    }
}

// === impl Stack ===

impl<T, M> svc::Stack<T> for Stack<M>
where
    T: Clone + Send + Sync + 'static,
    M: svc::Stack<T>,
{
    type Value = Service<T, M::Value>;
    type Error = M::Error;

    fn make(&self, t: &T) -> Result<Self::Value, Self::Error> {
        let target = t.clone();
        let inner = self.0.make(t)?;
        Ok(Service { inner, target })
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
