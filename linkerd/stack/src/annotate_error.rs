use crate::{layer, ExtractParam, NewService, Service};
use std::{
    error::Error,
    fmt,
    future::Future,
    marker::PhantomData,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

/// A [`NewService`] that extracts an [`ErrorContext`] implementation from its target,
/// and produces an [`AnnotateError`] middleware which wraps an inner [`Service`]'s
/// error type with that [`ErrorContext`].
pub struct NewAnnotateError<C, X, N> {
    inner: N,
    extract: X,
    _cx: PhantomData<fn(C)>,
}

/// Provides context to [`Error`]s.
///
/// This trait represents a type which can annotate an [`Error`] with
/// information about the context in which the error occurred, returning a
/// wrapped [`Self::Error`].
pub trait ErrorContext {
    /// The returned error type.
    type Error: Error + Send + Sync + 'static;

    /// Wraps `error` into a new [`Self::Error`], annotated with context
    /// provided by `self`.
    fn annotate<E>(&self, error: E) -> Self::Error
    where
        E: Into<linkerd_error::Error>;
}

/// A [`Service`] middleware which annotates its inner service's error type with
/// an [`ErrorContext`].
#[derive(Debug, Clone)]
pub struct AnnotateError<C, S> {
    inner: S,
    context: Arc<C>,
}

/// Futures returned by [`AnnotateError`].
#[derive(Debug)]
#[pin_project::pin_project]
pub struct ResponseFuture<F, C> {
    #[pin]
    f: F,
    context: Arc<C>,
}

/// An [`ErrorContext`] implementation that wraps errors in a new error type
/// that implements `From<(&T, E)>`, where `T` is a stack target.
pub struct FromTarget<T, E> {
    target: T,
    _err: PhantomData<fn(E)>,
}

type NewFromTarget<E, T, N> = NewAnnotateError<FromTarget<T, E>, fn(&T) -> FromTarget<T, E>, N>;

// === impl NewAnnotateError ===

impl<C: ErrorContext, N> NewAnnotateError<C, (), N> {
    /// Returns a `Layer` that produces [`NewAnnotateError`] middleware which
    /// annotate errors using an `C`-typed [`Param`](super::Param) type's
    /// [`ErrorContext`] implementation.
    pub fn layer() -> impl layer::Layer<N, Service = Self> + Clone {
        NewAnnotateError::layer_with(())
    }
}

impl<E, T, N> NewFromTarget<E, T, N> {
    /// Returns a `Layer` that produces [`NewAnnotateError`] middleware which
    /// annotate errors by constructing an `E`-typed error that implements
    /// `From<(&T, Error)>`, where `T` is a stack target.
    ///
    /// `T` must implement [`Clone`]
    pub fn layer_from_target() -> impl layer::Layer<N, Service = Self> + Clone
    where
        T: Clone,
        E: for<'a> From<(&'a T, linkerd_error::Error)>,
        E: Error + Send + Sync + 'static,
    {
        NewAnnotateError::layer_with(
            (|target: &T| FromTarget {
                target: target.clone(),
                _err: PhantomData,
            }) as fn(&T) -> FromTarget<T, E>,
        )
    }
}

impl<C, X, N> NewAnnotateError<C, X, N>
where
    C: ErrorContext,
    X: Clone,
{
    /// Returns a `Layer` that produces [`NewAnnotateError`] middleware which
    /// annotate errors using a `C`-typed [`ErrorContext`] implementation
    /// extracted from a stack  target using the provided [`ExtractParam`] implementation.
    pub fn layer_with(extract: X) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(inner, extract.clone()))
    }
}

impl<C, X, N> NewAnnotateError<C, X, N> {
    fn new(inner: N, extract: X) -> Self {
        Self {
            inner,
            extract,
            _cx: PhantomData,
        }
    }
}

impl<C, X, N> Clone for NewAnnotateError<C, X, N>
where
    N: Clone,
    X: Clone,
{
    fn clone(&self) -> Self {
        Self::new(self.inner.clone(), self.extract.clone())
    }
}

impl<C, X, N> fmt::Debug for NewAnnotateError<C, X, N>
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
        f.debug_struct("NewAnnotateError")
            .field("inner", inner)
            .field("extract", extract)
            .finish()
    }
}

impl<T, C, X, N> NewService<T> for NewAnnotateError<C, X, N>
where
    N: NewService<T>,
    X: ExtractParam<C, T>,
    C: ErrorContext,
{
    type Service = AnnotateError<C, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let context = Arc::new(self.extract.extract_param(&target));
        let inner = self.inner.new_service(target);

        AnnotateError { inner, context }
    }
}

// === impl AnnotateError ===

impl<S, C, Req> Service<Req> for AnnotateError<C, S>
where
    S: Service<Req>,
    S::Error: Into<linkerd_error::Error>,
    C: ErrorContext,
{
    type Response = S::Response;
    type Error = C::Error;
    type Future = ResponseFuture<S::Future, C>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner
            .poll_ready(cx)
            .map_err(|source| self.context.annotate(source))
    }

    fn call(&mut self, request: Req) -> Self::Future {
        ResponseFuture {
            f: self.inner.call(request),
            context: self.context.clone(),
        }
    }
}

// === impl ResponseFuture ===

impl<F, C, T, E> Future for ResponseFuture<F, C>
where
    F: Future<Output = Result<T, E>>,
    E: Into<linkerd_error::Error>,
    C: ErrorContext,
{
    type Output = Result<T, C::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        this.f
            .poll(cx)
            .map_err(|error| this.context.annotate(error))
    }
}

// === impl FromTarget ===

impl<T, E> ErrorContext for FromTarget<T, E>
where
    E: for<'a> From<(&'a T, linkerd_error::Error)>,
    E: Error + Send + Sync + 'static,
{
    type Error = E;
    fn annotate<E2>(&self, error: E2) -> Self::Error
    where
        E2: Into<linkerd_error::Error>,
    {
        E::from((&self.target, error.into()))
    }
}

impl<T: Clone, E> Clone for FromTarget<T, E> {
    fn clone(&self) -> Self {
        Self {
            target: self.target.clone(),
            _err: PhantomData,
        }
    }
}
