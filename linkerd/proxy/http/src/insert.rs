use futures::{ready, TryFuture};
use linkerd_stack::{layer, Proxy};
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

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

pub struct Insert<S, L, V> {
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
    F: Fn() -> V + Clone,
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

impl<S, L, V> tower::layer::Layer<S> for Layer<L, V>
where
    L: Lazy<V>,
    V: Send + Sync + 'static,
{
    type Service = Insert<S, L, V>;

    fn layer(&self, inner: S) -> Self::Service {
        Insert::new(inner, self.lazy.clone())
    }
}

// === impl Insert ===

impl<S, L, V> Insert<S, L, V> {
    fn new(inner: S, lazy: L) -> Self {
        Self {
            inner,
            lazy,
            _marker: PhantomData,
        }
    }
}

impl<P, S, L, V, B> Proxy<http::Request<B>, S> for Insert<P, L, V>
where
    P: Proxy<http::Request<B>, S>,
    S: tower::Service<P::Request>,
    L: Lazy<V>,
    V: Clone + Send + Sync + 'static,
{
    type Request = P::Request;
    type Response = P::Response;
    type Error = P::Error;
    type Future = P::Future;

    fn proxy(&self, svc: &mut S, mut req: http::Request<B>) -> Self::Future {
        req.extensions_mut().insert(self.lazy.value());
        self.inner.proxy(svc, req)
    }
}

impl<S, L, V, B> tower::Service<http::Request<B>> for Insert<S, L, V>
where
    S: tower::Service<http::Request<B>>,
    L: Lazy<V>,
    V: Clone + Send + Sync + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        req.extensions_mut().insert(self.lazy.value());
        self.inner.call(req)
    }
}

impl<S: Clone, L: Clone, V> Clone for Insert<S, L, V> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            lazy: self.lazy.clone(),
            _marker: self._marker,
        }
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
    use linkerd_stack as stack;
    use pin_project::pin_project;

    /// Wraps an HTTP `Service` so that the Stack's `T -typed target` is cloned into
    /// each request's extensions.
    #[derive(Clone, Debug)]
    pub struct NewService<M>(M);

    #[pin_project]
    pub struct MakeFuture<F, T> {
        #[pin]
        inner: F,
        target: T,
    }

    // === impl Layer ===

    pub fn layer<M>() -> impl tower::layer::Layer<M, Service = NewService<M>> + Copy {
        layer::mk(NewService)
    }

    // === impl Stack ===

    impl<T, M> stack::NewService<T> for NewService<M>
    where
        T: Clone + Send + Sync + 'static,
        M: stack::NewService<T>,
    {
        type Service = Insert<M::Service, super::ValLazy<T>, T>;

        fn new_service(&mut self, target: T) -> Self::Service {
            let inner = self.0.new_service(target.clone());
            super::Insert::new(inner, super::ValLazy(target))
        }
    }

    impl<T, M> tower::Service<T> for NewService<M>
    where
        T: Clone + Send + Sync + 'static,
        M: tower::Service<T>,
    {
        type Response = Insert<M::Response, super::ValLazy<T>, T>;
        type Error = M::Error;
        type Future = MakeFuture<M::Future, T>;

        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.0.poll_ready(cx)
        }

        fn call(&mut self, target: T) -> Self::Future {
            let inner = self.0.call(target.clone());
            MakeFuture { inner, target }
        }
    }

    // === impl MakeFuture ===

    impl<F, T> Future for MakeFuture<F, T>
    where
        F: TryFuture,
        T: Clone,
    {
        type Output = Result<Insert<F::Ok, ValLazy<T>, T>, F::Error>;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let this = self.project();
            let inner = ready!(this.inner.try_poll(cx))?;
            let svc = Insert::new(inner, super::ValLazy(this.target.clone()));
            Poll::Ready(Ok(svc))
        }
    }
}
