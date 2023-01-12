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

/// Wraps the [`Service`]s produced by an inner [`NewService`] with
/// [`AnnotateError`] middleware that enriches errors returned by those
/// [`Service`]s with a formatted context based on a `C`-typed `Param` extracted
/// from the `target` type used to construct that [`Service`].
///
/// A type implementing [`ExtractParam`] may be used to configure how the
/// formatted context is generated from the `target` type.
pub struct NewAnnotateError<N, C, X = ()> {
    inner: N,
    extract: X,
    _cx: PhantomData<fn(C)>,
}

/// Annotates errors returned by an inner [`Service`] with context formatted
/// from the `target` that this stack was constructed from.
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

/// An [`ExtractParam`] implementation which wraps a param type in [`Named`],
/// adding a string prefix to its [`fmt::Display`] representation.
#[derive(Clone, Debug)]
pub struct ExtractNamed {
    name: &'static str,
}

/// Wraps a `param` which implements [`fmt::Display`] with a string prefix
/// describing the stack which produced an error.
///
/// The resulting `Named` param's [`fmt::Display`] implementation will output
/// the following format:
/// ```
/// "{name} ({param})"
/// ```
pub fn named<P>(name: &'static str, param: P) -> Named<P> {
    Named { name, param }
}

// === impl NewAnnotateError ===

impl<N, C: fmt::Display> NewAnnotateError<N, C> {
    /// Returns a `Layer` that produces a `NewAnnotateError` which annotates
    /// errors with the formatted representation of a `C`-typed param extracted
    /// from the target.
    pub fn layer() -> impl layer::Layer<N, Service = Self> + Clone {
        NewAnnotateError::layer_with(())
    }

    /// Returns a `Layer` that produces a `NewAnnotateError` which annotates
    /// errors with the formatted representation of a `C`-typed param extracted
    /// from the target, prefixed with the provided `name`.
    pub fn layer_named(
        name: &'static str,
    ) -> impl layer::Layer<N, Service = NewAnnotateError<N, Named<C>, ExtractNamed>> + Clone {
        NewAnnotateError::layer_with(ExtractNamed { name })
    }
}

impl<N, C, X> NewAnnotateError<N, C, X>
where
    C: fmt::Display,
    X: Clone,
{
    /// Returns a `Layer` that produces a `NewAnnotateError` which annotates
    /// errors with the formatted representation of a `C`-typed param extracted
    /// from the target by the provided [`ExtractParam`] implementation.
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
        write!(f, "{} ({})", self.name, self.param)
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
