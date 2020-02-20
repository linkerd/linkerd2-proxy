use futures::{Future, Poll};
use linkerd2_error::Error;
use tower::util::{Either, Oneshot, ServiceExt};
use tracing::debug;

/// A fallback layer composing two service builders.
///
/// If the future returned by the primary builder's `MakeService` fails with
/// an error matching a given predicate, the fallback future will attempt
/// to call the secondary `MakeService`.
#[derive(Clone, Debug)]
pub struct FallbackLayer<F, P = fn(&Error) -> bool> {
    fallback: F,
    predicate: P,
}

#[derive(Clone, Debug)]
pub struct Fallback<I, F, P = fn(&Error) -> bool> {
    inner: I,
    fallback: F,
    predicate: P,
}

pub enum MakeFuture<A, B, P> {
    A {
        primary: A,
        fallback: Option<B>,
        predicate: P,
    },
    B(B),
}

// === impl FallbackLayer ===

impl<B> FallbackLayer<B> {
    pub fn new(fallback: B) -> Self {
        let predicate: fn(&Error) -> bool = |_| true;
        Self {
            fallback,
            predicate,
        }
    }

    /// Returns a `Layer` that uses the given `predicate` to determine whether
    /// to fall back.
    pub fn with_predicate<P>(self, predicate: P) -> FallbackLayer<B, P>
    where
        P: Fn(&Error) -> bool + Clone,
    {
        FallbackLayer {
            fallback: self.fallback,
            predicate,
        }
    }

    /// Returns a `Layer` that falls back if the error or its source is of
    /// type `E`.
    pub fn on_error<E>(self) -> FallbackLayer<B>
    where
        E: std::error::Error + 'static,
    {
        self.with_predicate(|e| e.is::<E>() || e.source().map(|s| s.is::<E>()).unwrap_or(false))
    }
}

impl<A, B, P> tower::layer::Layer<A> for FallbackLayer<B, P>
where
    B: Clone,
    P: Clone,
{
    type Service = Fallback<A, B, P>;

    fn layer(&self, inner: A) -> Self::Service {
        Self::Service {
            inner,
            fallback: self.fallback.clone(),
            predicate: self.predicate.clone(),
        }
    }
}

// === impl Fallback ===

impl<A, B, P, T> tower::Service<T> for Fallback<A, B, P>
where
    T: Clone,
    A: tower::Service<T>,
    A::Error: Into<Error>,
    B: tower::Service<T> + Clone,
    B::Error: Into<Error>,
    P: Fn(&Error) -> bool,
    P: Clone,
{
    type Response = Either<A::Response, B::Response>;
    type Error = Error;
    type Future = MakeFuture<A::Future, Oneshot<B, T>, P>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, target: T) -> Self::Future {
        MakeFuture::A {
            primary: self.inner.call(target.clone()),
            fallback: Some(self.fallback.clone().oneshot(target)),
            predicate: self.predicate.clone(),
        }
    }
}

// === impl MakeFuture ===

impl<A, B, P> Future for MakeFuture<A, B, P>
where
    A: Future,
    A::Error: Into<Error>,
    B: Future,
    B::Error: Into<Error>,
    P: Fn(&Error) -> bool,
{
    type Item = Either<A::Item, B::Item>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            *self = match self {
                MakeFuture::A {
                    ref mut primary,
                    ref mut fallback,
                    ref predicate,
                } => match primary.poll() {
                    Ok(ok) => return Ok(ok.map(Either::A)),
                    Err(e) => {
                        let error = e.into();
                        if !(predicate)(&error) {
                            return Err(error);
                        }

                        debug!(%error, "Falling back");
                        MakeFuture::B(fallback.take().unwrap())
                    }
                },
                MakeFuture::B(ref mut b) => {
                    return b.poll().map(|ok| ok.map(Either::B)).map_err(Into::into);
                }
            };
        }
    }
}
