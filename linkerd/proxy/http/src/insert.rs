use futures::{Future, TryFuture};
use linkerd_stack::{layer, NewService, Param, Proxy};
use std::{
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};

pub trait Lazy<V>: Clone {
    fn value(&self) -> V;
}

/// Wraps an HTTP `Service` so that a `P`-typed `Param` is cloned into each
/// request's extensions.
#[derive(Debug)]
pub struct NewInsert<P, N> {
    inner: N,
    _marker: PhantomData<fn() -> P>,
}

/// Wraps an HTTP `Service` so that a `P`-typed `Param` is cloned into each
/// response's extensions.
#[derive(Debug)]
pub struct NewResponseInsert<P, N> {
    inner: N,
    _marker: PhantomData<fn() -> P>,
}

pub struct Insert<S, L, V> {
    inner: S,
    lazy: L,
    _marker: PhantomData<fn() -> V>,
}

pub struct ResponseInsert<S, L, V> {
    inner: S,
    lazy: L,
    _marker: PhantomData<fn() -> V>,
}

#[derive(Clone, Debug)]
pub struct FnLazy<F>(F);

#[derive(Clone, Debug)]
pub struct ValLazy<V>(V);

#[pin_project::pin_project]
#[derive(Debug)]
pub struct ResponseInsertFuture<F, L, V, B> {
    #[pin]
    inner: F,
    lazy: L,
    _marker: PhantomData<fn(B) -> V>,
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

    fn new_service(&self, target: T) -> Self::Service {
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

// === impl NewResponseInsert ===

impl<P, N> NewResponseInsert<P, N> {
    pub fn layer() -> impl tower::layer::Layer<N, Service = Self> + Copy {
        layer::mk(|inner| Self {
            inner,
            _marker: PhantomData,
        })
    }
}

impl<T, P, N> NewService<T> for NewResponseInsert<P, N>
where
    T: Param<P>,
    P: Clone + Send + Sync + 'static,
    N: NewService<T>,
{
    type Service = ResponseInsert<N::Service, ValLazy<P>, P>;

    #[inline]
    fn new_service(&self, target: T) -> Self::Service {
        let param = target.param();
        let inner = self.inner.new_service(target);
        ResponseInsert::new(inner, ValLazy(param))
    }
}

impl<N: Clone, P> Clone for NewResponseInsert<P, N> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _marker: self._marker,
        }
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

    #[inline]
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

// === impl ResponseInsert ===

impl<S, L, V> ResponseInsert<S, L, V> {
    fn new(inner: S, lazy: L) -> Self {
        Self {
            inner,
            lazy,
            _marker: PhantomData,
        }
    }
}

impl<Req, P, S, L, V, B> Proxy<Req, S> for ResponseInsert<P, L, V>
where
    P: Proxy<Req, S, Response = http::Response<B>>,
    S: tower::Service<P::Request>,
    L: Lazy<V>,
    V: Clone + Send + Sync + 'static,
{
    type Request = P::Request;
    type Response = P::Response;
    type Error = P::Error;
    type Future = ResponseInsertFuture<P::Future, L, V, B>;

    fn proxy(&self, svc: &mut S, req: Req) -> Self::Future {
        ResponseInsertFuture {
            inner: self.inner.proxy(svc, req),
            lazy: self.lazy.clone(),
            _marker: PhantomData,
        }
    }
}

impl<Req, S, L, V, B> tower::Service<Req> for ResponseInsert<S, L, V>
where
    S: tower::Service<Req, Response = http::Response<B>>,
    L: Lazy<V>,
    V: Clone + Send + Sync + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = ResponseInsertFuture<S::Future, L, V, B>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        ResponseInsertFuture {
            inner: self.inner.call(req),
            lazy: self.lazy.clone(),
            _marker: PhantomData,
        }
    }
}

impl<S: Clone, L: Clone, V> Clone for ResponseInsert<S, L, V> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            lazy: self.lazy.clone(),
            _marker: self._marker,
        }
    }
}

// === impl ResponseInsertFuture ===

impl<F, L, V, B> Future for ResponseInsertFuture<F, L, V, B>
where
    F: TryFuture<Ok = http::Response<B>>,
    L: Lazy<V>,
    V: Send + Sync + 'static,
{
    type Output = Result<F::Ok, F::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let mut rsp = futures::ready!(this.inner.try_poll(cx))?;
        rsp.extensions_mut().insert(this.lazy.value());
        Poll::Ready(Ok(rsp))
    }
}

// === impl ValLazy ===

impl<V> Lazy<V> for ValLazy<V>
where
    V: Clone + Send + Sync + 'static,
{
    fn value(&self) -> V {
        self.0.clone()
    }
}

// === impl FnLazy ===

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
