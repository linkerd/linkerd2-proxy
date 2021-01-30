use linkerd_stack::{layer, NewService, Param, Proxy};
use std::marker::PhantomData;
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

/// Wraps an HTTP `Service` so that a `P`-typed `Param` is cloned into each
/// request's extensions.
#[derive(Debug)]
pub struct NewInsert<P, N> {
    inner: N,
    _marker: PhantomData<fn() -> P>,
}

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

// === impl NewInsert ===

impl<P, N> NewInsert<P, N> {
    pub fn layer() -> impl tower::layer::Layer<N, Service = Self> + Copy {
        layer::mk(|inner| Self {
            inner,
            _marker: PhantomData,
        })
    }
}

impl<T, P, N> NewService<T> for NewInsert<P, N>
where
    T: Param<P>,
    P: Clone + Send + Sync + 'static,
    N: NewService<T>,
{
    type Service = Insert<N::Service, ValLazy<P>, P>;

    fn new_service(&mut self, target: T) -> Self::Service {
        let param = target.param();
        let inner = self.inner.new_service(target);
        Insert::new(inner, ValLazy(param))
    }
}

impl<N: Clone, P> Clone for NewInsert<P, N> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _marker: self._marker,
        }
    }
}
