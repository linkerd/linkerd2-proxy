use proxy::http::router::rt;
use std::{fmt, marker::PhantomData};

use futures::{Future, Poll};
use tower::buffer::Buffer;

use logging;
use svc;

type Error = Box<dyn std::error::Error + Send + Sync>;

/// Produces `MakeService`s where the output `Service` is wrapped with a `Buffer`
#[derive(Debug)]
pub struct Layer<Req> {
    capacity: usize,
    _marker: PhantomData<fn(Req)>,
}

/// Produces `MakeService`s where the output `Service` is wrapped with a `Buffer`
#[derive(Debug)]
pub struct MakeBuffer<M, Req> {
    capacity: usize,
    inner: M,
    _marker: PhantomData<fn(Req)>,
}

pub struct MakeFuture<F, T, Req> {
    capacity: usize,
    executor: logging::ContextualExecutor<T>,
    inner: F,
    _marker: PhantomData<fn(Req)>,
}

// === impl Layer ===

pub fn layer<Req>(capacity: usize) -> Layer<Req> {
    Layer {
        capacity,
        _marker: PhantomData,
    }
}

impl<Req> Clone for Layer<Req> {
    fn clone(&self) -> Self {
        Layer {
            capacity: self.capacity,
            _marker: PhantomData,
        }
    }
}

impl<M, Req> svc::Layer<M> for Layer<Req> {
    type Service = MakeBuffer<M, Req>;

    fn layer(&self, inner: M) -> Self::Service {
        MakeBuffer {
            capacity: self.capacity,
            inner,
            _marker: PhantomData,
        }
    }
}

// === impl MakeBuffer ===

impl<M: Clone, Req> Clone for MakeBuffer<M, Req> {
    fn clone(&self) -> Self {
        MakeBuffer {
            capacity: self.capacity,
            inner: self.inner.clone(),
            _marker: PhantomData,
        }
    }
}

impl<T, M, Req> svc::Service<T> for MakeBuffer<M, Req>
where
    T: fmt::Display + Clone + Send + Sync + 'static,
    M: svc::Service<T>,
    M::Response: svc::Service<Req> + Send + 'static,
    M::Error: Into<Error>,
    <M::Response as svc::Service<Req>>::Future: Send,
    <M::Response as svc::Service<Req>>::Error: Into<Error>,
    Req: Send + 'static,
{
    type Response = Buffer<M::Response, Req>;
    type Error = Error;
    type Future = MakeFuture<M::Future, T, Req>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, target: T) -> Self::Future {
        let executor = logging::context_executor(target.clone());
        let inner = self.inner.call(target);

        MakeFuture {
            capacity: self.capacity,
            executor,
            inner,
            _marker: PhantomData,
        }
    }
}

impl<T, M, S, Req> rt::Make<T> for MakeBuffer<M, Req>
where
    T: fmt::Display + Clone + Send + Sync + 'static,
    M: rt::Make<T, Value = S>,
    S: svc::Service<Req> + Send + 'static,
    S::Future: Send,
    S::Error: Into<Error>,
    Req: Send + 'static,
{
    type Value = Buffer<S, Req>;

    fn make(&self, target: &T) -> Self::Value {
        let mut executor = logging::context_executor(target.clone());
        let svc = self.inner.make(target);

        Buffer::with_executor(svc, self.capacity, &mut executor)
    }
}

impl<M, Req> MakeBuffer<M, Req> {
    /// Creates a buffer immediately.
    pub fn make<T>(&self, target: T) -> Buffer<M::Value, Req>
    where
        T: fmt::Display + Clone + Send + Sync + 'static,
        M: rt::Make<T>,
        M::Value: svc::Service<Req> + Send + 'static,
        <M::Value as svc::Service<Req>>::Future: Send,
        <M::Value as svc::Service<Req>>::Error: Into<Error>,
        Req: Send + 'static,
    {
        let mut executor = logging::context_executor(target.clone());
        let svc = self.inner.make(&target);

        Buffer::with_executor(svc, self.capacity, &mut executor)
    }
}

// === impl MakeFuture ===

impl<F, T, Req, Svc> Future for MakeFuture<F, T, Req>
where
    F: Future<Item = Svc>,
    F::Error: Into<Error>,
    Svc: svc::Service<Req> + Send + 'static,
    Svc::Future: Send,
    Svc::Error: Into<Error>,
    Req: Send + 'static,
    T: fmt::Display + Send + Sync + 'static,
{
    type Item = Buffer<Svc, Req>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let svc = try_ready!(self.inner.poll().map_err(Into::into));

        let buffer = Buffer::with_executor(svc, self.capacity, &mut self.executor);

        Ok(buffer.into())
    }
}
