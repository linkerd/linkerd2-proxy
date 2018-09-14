use futures::Poll;
use http;
use std::marker::PhantomData;

use svc;

/// Wraps HTTP `Service` `Stack<T>`s so that `T` is cloned into each request's
/// extensions.
#[derive(Debug)]
pub struct Layer<T, M>(PhantomData<fn() -> (T, M)>);

/// Wraps an HTTP `Service` so that the Stack's `T -typed target` is cloned into
/// each request's extensions.
#[derive(Clone, Debug)]
pub struct Stack<T, M>(M, PhantomData<fn() -> T>);

#[derive(Clone, Debug)]
pub struct Service<T, S> {
    target: T,
    inner: S,
}

// === impl Layer ===

impl<T, M> Layer<T, M> {
    pub fn new() -> Self {
        Layer(PhantomData)
    }
}

impl<T, M> Clone for Layer<T, M> {
    fn clone(&self) -> Self {
        Self::new()
    }
}

impl<T, M, B> svc::Layer<T, T, M> for Layer<T, M>
where
    T: Clone + Send + Sync + 'static,
    M: svc::Stack<T>,
    M::Value: svc::Service<Request = http::Request<B>>,
{
    type Value = <Stack<T, M> as svc::Stack<T>>::Value;
    type Error = <Stack<T, M> as svc::Stack<T>>::Error;
    type Stack = Stack<T, M>;

    fn bind(&self, next: M) -> Self::Stack {
        Stack(next, PhantomData)
    }
}

// === impl Stack ===

impl<T, M, B> svc::Stack<T> for Stack<T, M>
where
    T: Clone + Send + Sync + 'static,
    M: svc::Stack<T>,
    M::Value: svc::Service<Request = http::Request<B>>,
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

impl<T, S, B> svc::Service for Service<T, S>
where
    T: Clone + Send + Sync + 'static,
    S: svc::Service<Request = http::Request<B>>,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, mut req: Self::Request) -> Self::Future {
        req.extensions_mut().insert(self.target.clone());
        self.inner.call(req)
    }
}
