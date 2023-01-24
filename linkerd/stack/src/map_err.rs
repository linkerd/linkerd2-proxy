use crate::{layer, ExtractParam, NewService};
use linkerd_error::Error;
use std::{
    fmt,
    future::Future,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};

pub trait WrapErr<E> {
    type Error: Into<Error>;

    fn wrap_err(&self, error: E) -> Self::Error;
}

impl<E: Into<Error>> WrapErr<E> for () {
    type Error = Error;

    fn wrap_err(&self, error: E) -> Error {
        error.into()
    }
}

impl<InE, OutE, F> WrapErr<InE> for F
where
    F: FnOnce(InE) -> OutE + Clone,
    OutE: Into<Error>,
{
    type Error = OutE;

    fn wrap_err(&self, error: InE) -> OutE {
        (self.clone())(error)
    }
}

/// A [`NewService`] that extracts a [`WrapErr`] implementation from its target
/// and produces a [`MapErr`] middleware that wrapps inner error types.
pub struct NewMapErr<W, X, N> {
    inner: N,
    extract: X,
    _cx: PhantomData<fn(W)>,
}

/// Like `tower::util::MapErr`, but with an implementation of `Proxy`.
#[derive(Clone, Debug)]
pub struct MapErr<W, S> {
    inner: S,
    wrap: W,
}

/// Future returned by [`MapErr`].
#[derive(Debug)]
#[pin_project::pin_project]
pub struct ResponseFuture<W, F> {
    #[pin]
    inner: F,
    wrap: W,
}

/// A [`WrapErr`] that converts inner errors to an `E` via `From<(&T, Error)`,
/// where `T` is a stack target.
pub struct WrapFromTarget<T, E> {
    target: T,
    _err: PhantomData<fn(E)>,
}

// === impl NewMapErr===

impl<W, X, N> NewMapErr<W, X, N> {
    fn new(inner: N, extract: X) -> Self {
        Self {
            inner,
            extract,
            _cx: PhantomData,
        }
    }
}

impl<W, X: Clone, N> NewMapErr<W, X, N> {
    /// A `NewService` layer that extracts a `WrapErr` implementation via the
    /// `X`-typed `ExtractParam`.
    pub fn layer_with(extract: X) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(inner, extract.clone()))
    }
}

impl<W, N> NewMapErr<W, (), N> {
    /// A `NewService` layer that extracts a `W`-typed `WrapErr` implementation
    /// from each target.
    pub fn layer() -> impl layer::Layer<N, Service = Self> + Clone {
        NewMapErr::layer_with(())
    }
}

type ExtractWrapFromTarget<T, E> = fn(&T) -> WrapFromTarget<T, E>;

// We don't actually care about the `()` types here, but they help avoid
// inference problems.
impl<N> NewMapErr<(), (), N> {
    /// A `NewService` layer that converts inner errors to an `E`-typed error via
    /// `From<(&T, Error)`, where `T` is a stack target.
    pub fn layer_from_target<E, T>() -> impl layer::Layer<
        N,
        Service = NewMapErr<WrapFromTarget<T, E>, ExtractWrapFromTarget<T, E>, N>,
    > + Clone
    where
        T: Clone,
        WrapFromTarget<T, E>: WrapErr<Error> + Clone,
    {
        let extract = |t: &T| WrapFromTarget {
            target: t.clone(),
            _err: PhantomData,
        };
        NewMapErr::layer_with(extract as ExtractWrapFromTarget<T, E>)
    }
}

impl<T, W, X, N> NewService<T> for NewMapErr<W, X, N>
where
    X: ExtractParam<W, T>,
    N: NewService<T>,
{
    type Service = MapErr<W, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let wrap = self.extract.extract_param(&target);
        let inner = self.inner.new_service(target);

        MapErr { inner, wrap }
    }
}

impl<W, X, N> Clone for NewMapErr<W, X, N>
where
    N: Clone,
    X: Clone,
{
    fn clone(&self) -> Self {
        Self::new(self.inner.clone(), self.extract.clone())
    }
}

impl<W, X, N> fmt::Debug for NewMapErr<W, X, N>
where
    N: fmt::Debug,
    X: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            inner,
            extract,
            _cx,
        } = self;
        f.debug_struct("NewMapErr")
            .field("inner", inner)
            .field("extract", extract)
            .finish()
    }
}

// === impl MapErr ===

impl<W, S> MapErr<W, S> {
    pub fn new(inner: S, wrap: W) -> Self {
        Self { inner, wrap }
    }

    pub fn layer(wrap: W) -> impl super::layer::Layer<S, Service = Self> + Clone
    where
        W: Clone,
    {
        super::layer::mk(move |inner| Self::new(inner, wrap.clone()))
    }
}

impl<S> MapErr<(), S> {
    pub fn layer_boxed() -> impl layer::Layer<S, Service = Self> + Clone {
        MapErr::layer(())
    }
}

impl<Req, W, S> super::Service<Req> for MapErr<W, S>
where
    S: super::Service<Req>,
    W: WrapErr<S::Error> + Clone,
{
    type Response = S::Response;
    type Error = W::Error;
    type Future = ResponseFuture<W, S::Future>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(|e| self.wrap.wrap_err(e))
    }

    #[inline]
    fn call(&mut self, req: Req) -> Self::Future {
        let inner = self.inner.call(req);
        ResponseFuture {
            inner,
            wrap: self.wrap.clone(),
        }
    }
}

impl<Req, W, P, S> super::Proxy<Req, S> for MapErr<W, P>
where
    W: WrapErr<P::Error> + Clone,
    P: super::Proxy<Req, S>,
    S: super::Service<P::Request>,
{
    type Request = P::Request;
    type Response = P::Response;
    type Error = W::Error;
    type Future = ResponseFuture<W, P::Future>;

    #[inline]
    fn proxy(&self, inner: &mut S, req: Req) -> Self::Future {
        let inner = self.inner.proxy(inner, req);
        ResponseFuture {
            inner,
            wrap: self.wrap.clone(),
        }
    }
}

// === impl ResponseFuture ===

impl<T, E, W, F> Future for ResponseFuture<W, F>
where
    W: WrapErr<E>,
    F: Future<Output = Result<T, E>>,
{
    type Output = Result<T, W::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        this.inner
            .poll(cx)
            .map_err(|error| this.wrap.wrap_err(error))
    }
}

// === impl WrapFromTarget ===

impl<T, InE, OutE> WrapErr<InE> for WrapFromTarget<T, OutE>
where
    InE: Into<Error>,
    OutE: for<'a> From<(&'a T, Error)>,
    OutE: Into<Error>,
{
    type Error = OutE;

    fn wrap_err(&self, error: InE) -> OutE {
        OutE::from((&self.target, error.into()))
    }
}

impl<T: fmt::Debug, E> fmt::Debug for WrapFromTarget<T, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WrapFromTarget")
            .field("target", &self.target)
            .finish()
    }
}

impl<T: Clone, E> Clone for WrapFromTarget<T, E> {
    fn clone(&self) -> Self {
        Self {
            target: self.target.clone(),
            _err: PhantomData,
        }
    }
}
