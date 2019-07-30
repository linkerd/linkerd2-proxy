use futures::{Future, Poll};
use http;
use std::marker::PhantomData;

use svc;

pub trait Lazy<V>: Clone {
    fn value(&self) -> V;
}

/// Wraps an HTTP `Service` so that the `T -typed value` is cloned into
/// each request's extensions.
#[derive(Clone, Debug)]
pub struct Layer<L, V> {
    lazy: L,
    _marker: PhantomData<fn() -> V>,
}

#[derive(Clone)]
pub struct Make<M, L, V> {
    inner: M,
    lazy: L,
    _marker: PhantomData<fn() -> V>,
}

pub struct MakeFuture<F, L, V> {
    inner: F,
    lazy: L,
    _marker: PhantomData<fn() -> V>,
}

#[derive(Clone)]
pub struct Service<S, L, V> {
    inner: S,
    lazy: L,
    _marker: PhantomData<fn() -> V>,
}

#[derive(Clone, Debug)]
pub struct FnLazy<F>(F);

#[derive(Clone, Debug)]
pub struct ValLazy<V>(V);

pub fn layer<F, V>(f: F) -> Layer<FnLazy<F>, V>
where
    F: Fn() -> V,
    V: Send + Sync + 'static,
{
    Layer::new(FnLazy(f))
}

// === impl Layer ===

impl<L, V> Layer<L, V>
where
    L: Lazy<V>,
    V: Send + Sync + 'static,
{
    pub fn new(lazy: L) -> Self {
        Self {
            lazy,
            _marker: PhantomData,
        }
    }
}

impl<M, L, V> svc::Layer<M> for Layer<L, V>
where
    L: Lazy<V>,
    V: Send + Sync + 'static,
{
    type Service = Make<M, L, V>;

    fn layer(&self, inner: M) -> Self::Service {
        Self::Service {
            inner,
            lazy: self.lazy.clone(),
            _marker: PhantomData,
        }
    }
}

// === impl Make ===

impl<T, M, L, V> svc::Service<T> for Make<M, L, V>
where
    M: svc::Service<T>,
    L: Lazy<V>,
    V: Send + Sync + 'static,
{
    type Response = Service<M::Response, L, V>;
    type Error = M::Error;
    type Future = MakeFuture<M::Future, L, V>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, t: T) -> Self::Future {
        Self::Future {
            inner: self.inner.call(t),
            lazy: self.lazy.clone(),
            _marker: PhantomData,
        }
    }
}

// === impl MakeFuture ===

impl<F, L, V> Future for MakeFuture<F, L, V>
where
    F: Future,
    L: Lazy<V>,
    V: Send + Sync + 'static,
{
    type Item = Service<F::Item, L, V>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());
        let svc = Service::new(inner, self.lazy.clone());
        Ok(svc.into())
    }
}

// === impl Service ===

impl<S, L, V> Service<S, L, V> {
    fn new(inner: S, lazy: L) -> Self {
        Self {
            inner,
            lazy,
            _marker: PhantomData,
        }
    }
}

impl<S, L, V, B> svc::Service<http::Request<B>> for Service<S, L, V>
where
    S: svc::Service<http::Request<B>>,
    L: Lazy<V>,
    V: Clone + Send + Sync + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        req.extensions_mut().insert(self.lazy.value());
        self.inner.call(req)
    }
}

impl<V> Lazy<V> for ValLazy<V>
where
    V: Clone + Send + Sync + 'static,
{
    fn value(&self) -> V {
        self.0.clone()
    }
}

impl<F, V> Lazy<V> for FnLazy<F>
where
    F: Fn() -> V,
    F: Clone,
    V: Send + Sync + 'static,
{
    fn value(&self) -> V {
        (self.0)()
    }
}

pub mod target {
    use super::*;

    /// Wraps an HTTP `Service` so that the Stack's `T -typed target` is cloned into
    /// each request's extensions.
    #[derive(Clone, Debug)]
    pub struct Make<M>(M);

    pub struct MakeFuture<F, T> {
        inner: F,
        target: T,
    }

    // === impl Layer ===

    pub fn layer<M>() -> impl svc::Layer<M, Service = Make<M>> + Copy {
        svc::layer::mk(Make)
    }

    // === impl Stack ===

    impl<T, M> svc::Service<T> for Make<M>
    where
        T: Clone + Send + Sync + 'static,
        M: svc::Service<T>,
    {
        type Response = super::Service<M::Response, super::ValLazy<T>, T>;
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
        type Item = super::Service<F::Item, ValLazy<T>, T>;
        type Error = F::Error;

        fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
            let inner = try_ready!(self.inner.poll());
            let svc = super::Service::new(inner, super::ValLazy(self.target.clone()));
            Ok(svc.into())
        }
    }
}
