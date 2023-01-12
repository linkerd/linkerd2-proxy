use crate::{layer, ExtractParam, NewService, Param, Service};
use std::{
    error::Error,
    fmt,
    future::Future,
    marker::PhantomData,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

pub struct NewAnnotateError<N, C, X = ()> {
    inner: N,
    extract: X,
    _cx: PhantomData<fn(C)>,
}

#[derive(Debug, Clone)]
pub struct AnnotateError<S> {
    inner: S,
    context: Arc<str>,
}

#[derive(Debug)]
pub struct AnnotatedError {
    context: Arc<str>,
    // TODO(eliza): avoid double boxing these
    source: linkerd_error::Error,
}

#[derive(Debug)]
#[pin_project::pin_project]
pub struct ResponseFuture<F> {
    #[pin]
    f: F,
    context: Option<Arc<str>>,
}

#[derive(Clone, Debug)]
pub struct Named<P> {
    name: &'static str,
    param: P,
}

#[derive(Clone, Debug)]
pub struct ExtractNamed {
    name: &'static str,
}

// === impl NewAnnotateError ===

impl<N, C: fmt::Display> NewAnnotateError<N, C> {
    pub fn layer() -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(|inner| Self::new(inner, ()))
    }

    pub fn layer_named(
        name: &'static str,
    ) -> impl layer::Layer<N, Service = NewAnnotateError<N, Named<C>, ExtractNamed>> + Clone {
        layer::mk(move |inner| NewAnnotateError::new(inner, ExtractNamed { name }))
    }
}

impl<N, C, X> NewAnnotateError<N, C, X>
where
    C: fmt::Display,
    X: Clone,
{
    pub fn layer_with(extract: X) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(inner, extract.clone()))
    }
}

impl<N, C> NewAnnotateError<N, Named<C>, ExtractNamed> where C: fmt::Display {}

impl<N, C, X> NewAnnotateError<N, C, X> {
    fn new(inner: N, extract: X) -> Self {
        Self {
            inner,
            extract,
            _cx: PhantomData,
        }
    }
}

impl<N, C, X> Clone for NewAnnotateError<N, C, X>
where
    N: Clone,
    X: Clone,
{
    fn clone(&self) -> Self {
        Self::new(self.inner.clone(), self.extract.clone())
    }
}

impl<N, C, X> fmt::Debug for NewAnnotateError<N, C, X>
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

impl<T, N, C, X> NewService<T> for NewAnnotateError<N, C, X>
where
    N: NewService<T>,
    X: ExtractParam<C, T>,
    C: fmt::Display,
{
    type Service = AnnotateError<N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let param = self.extract.extract_param(&target);
        let context = param.to_string().into();
        let inner = self.inner.new_service(target);

        AnnotateError { inner, context }
    }
}

// === impl AnnotateError ===

impl<S, Req> Service<Req> for AnnotateError<S>
where
    S: Service<Req>,
    S::Error: Into<linkerd_error::Error>,
{
    type Response = S::Response;
    type Error = linkerd_error::Error;
    type Future = ResponseFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(|source| {
            AnnotatedError {
                context: self.context.clone(),
                source: source.into(),
            }
            .into()
        })
    }

    fn call(&mut self, request: Req) -> Self::Future {
        ResponseFuture {
            f: self.inner.call(request),
            context: Some(self.context.clone()),
        }
    }
}

// === impl AnnotatedError ===

impl fmt::Display for AnnotatedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { context, source } = self;
        write!(f, "{context}: {source}")
    }
}

impl Error for AnnotatedError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

impl<F, T, E> Future for ResponseFuture<F>
where
    F: Future<Output = Result<T, E>>,
    E: Into<linkerd_error::Error>,
{
    type Output = Result<T, linkerd_error::Error>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        this.f.poll(cx).map_err(|source| {
            AnnotatedError {
                context: this.context.take().expect("polled after ready"),
                source: source.into(),
            }
            .into()
        })
    }
}

// === impl Named ===

impl<P: fmt::Display> fmt::Display for Named<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.name, self.param)
    }
}

// === impl ExtractNamed ===

impl<P, T: Param<P>> ExtractParam<Named<P>, T> for ExtractNamed {
    fn extract_param(&self, t: &T) -> Named<P> {
        Named {
            name: self.name,
            param: t.param(),
        }
    }
}
