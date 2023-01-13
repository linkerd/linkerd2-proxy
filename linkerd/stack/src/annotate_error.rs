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
pub struct NewAnnotateError<N, A, X = ()> {
    inner: N,
    extract: X,
    _cx: PhantomData<fn(A)>,
}

pub trait AnnotateError {
    type Error: Error + Send + Sync + 'static;
    fn annotate<E>(&self, error: E) -> Self::Error
    where
        E: Into<linkerd_error::Error>;
}

#[derive(Debug, Clone)]
pub struct AnnotateErrorService<A, S> {
    inner: S,
    annotate: Arc<A>,
}

#[derive(Debug)]
#[pin_project::pin_project]
pub struct ResponseFuture<F, A> {
    #[pin]
    f: F,
    annotate: Arc<A>,
}

#[derive(Debug)]
#[pin_project::pin_project]
pub struct MakeServiceFuture<F, A> {
    #[pin]
    f: F,
    annotate: Arc<A>,
}

pub struct FromTarget<T, E> {
    target: T,
    _err: PhantomData<fn(E)>,
}

// === impl NewAnnotateError ===

pub fn layer_from_target<E, T, N>(
) -> impl layer::Layer<N, Service = NewAnnotateError<N, FromTarget<T, E>, fn(&T) -> FromTarget<T, E>>>
       + Clone
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

impl<N, A: AnnotateError> NewAnnotateError<N, A> {
    pub fn layer() -> impl layer::Layer<N, Service = Self> + Clone {
        NewAnnotateError::layer_with(())
    }
}

impl<N, A, X> NewAnnotateError<N, A, X>
where
    A: AnnotateError,
    X: Clone,
{
    pub fn layer_with(extract: X) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(inner, extract.clone()))
    }
}

impl<N, A, X> NewAnnotateError<N, A, X> {
    fn new(inner: N, extract: X) -> Self {
        Self {
            inner,
            extract,
            _cx: PhantomData,
        }
    }
}

impl<N, A, X> Clone for NewAnnotateError<N, A, X>
where
    N: Clone,
    X: Clone,
{
    fn clone(&self) -> Self {
        Self::new(self.inner.clone(), self.extract.clone())
    }
}

impl<N, A, X> fmt::Debug for NewAnnotateError<N, A, X>
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

impl<T, N, A, X> NewService<T> for NewAnnotateError<N, A, X>
where
    N: NewService<T>,
    X: ExtractParam<A, T>,
    A: AnnotateError,
{
    type Service = AnnotateErrorService<A, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let annotate = Arc::new(self.extract.extract_param(&target));
        let inner = self.inner.new_service(target);

        AnnotateErrorService { inner, annotate }
    }
}

impl<S, A, T, X> Service<T> for NewAnnotateError<S, A, X>
where
    S: Service<T>,
    S::Error: Into<linkerd_error::Error>,
    X: ExtractParam<A, T>,
    A: AnnotateError,
{
    type Response = AnnotateErrorService<A, S::Response>;
    type Error = linkerd_error::Error;
    type Future = MakeServiceFuture<S::Future, A>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, target: T) -> Self::Future {
        let annotate = Arc::new(self.extract.extract_param(&target));
        MakeServiceFuture {
            f: self.inner.call(target),
            annotate,
        }
    }
}

// === impl AnnotateErrorService ===

impl<S, A, Req> Service<Req> for AnnotateErrorService<A, S>
where
    S: Service<Req>,
    S::Error: Into<linkerd_error::Error>,
    A: AnnotateError,
{
    type Response = S::Response;
    type Error = A::Error;
    type Future = ResponseFuture<S::Future, A>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner
            .poll_ready(cx)
            .map_err(|source| self.annotate.annotate(source))
    }

    fn call(&mut self, request: Req) -> Self::Future {
        ResponseFuture {
            f: self.inner.call(request),
            annotate: self.annotate.clone(),
        }
    }
}

// === impl ResponseFuture ===

impl<F, A, T, E> Future for ResponseFuture<F, A>
where
    F: Future<Output = Result<T, E>>,
    E: Into<linkerd_error::Error>,
    A: AnnotateError,
{
    type Output = Result<T, A::Error>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        this.f
            .poll(cx)
            .map_err(|error| this.annotate.annotate(error))
    }
}

// === impl MakeServiceFuture ===

impl<F, A, S, E> Future for MakeServiceFuture<F, A>
where
    F: Future<Output = Result<S, E>>,
    E: Into<linkerd_error::Error>,
    A: AnnotateError,
{
    type Output = Result<AnnotateErrorService<A, S>, linkerd_error::Error>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        this.f
            .poll(cx)
            .map_err(|error| this.annotate.annotate(error).into())
            .map_ok(|inner| AnnotateErrorService {
                inner,
                annotate: this.annotate.clone(),
            })
    }
}

// === impl FromTarget ===

impl<T, E> AnnotateError for FromTarget<T, E>
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
