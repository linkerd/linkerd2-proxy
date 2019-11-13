#![deny(warnings, rust_2018_idioms)]

use futures::{try_ready, Future, Poll};
use linkerd2_error::Error;
use tracing::trace;

/// A fallback layer composing two service builders.
///
/// If the future returned by the primary builder's `MakeService` fails with
/// an error matching a given predicate, the fallback future will attempt
/// to call the secondary `MakeService`.
#[derive(Clone, Debug)]
pub struct Layer<A, B, P = fn(&Error) -> bool> {
    primary: A,
    fallback: B,
    predicate: P,
}

#[derive(Clone, Debug)]
pub struct MakeSvc<A, B, P> {
    primary: A,
    fallback: B,
    predicate: P,
}

pub struct MakeFuture<A, B, P, T>
where
    A: Future,
    A::Error: Into<Error>,
    B: tower::Service<T>,
{
    fallback: B,
    target: Option<T>,
    predicate: P,
    state: FallbackState<A, B::Future, T>,
}

enum FallbackState<A, B, T> {
    /// Waiting for the primary service's future to complete.
    Primary(A),
    ///W aiting for the fallback service to become ready.
    Waiting(Option<T>),
    /// Waiting for the fallback service's future to complete.
    Fallback(B),
}

pub fn layer<A, B>(primary: A, fallback: B) -> Layer<A, B> {
    let predicate: fn(&Error) -> bool = |_| true;
    Layer {
        primary,
        fallback,
        predicate,
    }
}

// === impl Layer ===

impl<A, B> Layer<A, B> {
    /// Returns a `Layer` that uses the given `predicate` to determine whether
    /// to fall back.
    pub fn with_predicate<P>(self, predicate: P) -> Layer<A, B, P>
    where
        P: Fn(&Error) -> bool + Clone,
    {
        Layer {
            primary: self.primary,
            fallback: self.fallback,
            predicate,
        }
    }

    /// Returns a `Layer` that falls back if the error or its source is of
    /// type `E`.
    pub fn on_error<E>(self) -> Layer<A, B>
    where
        E: std::error::Error + 'static,
    {
        self.with_predicate(|e| e.is::<E>() || e.source().map(|s| s.is::<E>()).unwrap_or(false))
    }
}

impl<A, B, P, M> tower::layer::Layer<M> for Layer<A, B, P>
where
    A: tower::layer::Layer<M>,
    B: tower::layer::Layer<M>,
    M: Clone,
    P: Fn(&Error) -> bool + Clone,
{
    type Service = MakeSvc<A::Service, B::Service, P>;

    fn layer(&self, inner: M) -> Self::Service {
        MakeSvc {
            primary: self.primary.layer(inner.clone()),
            fallback: self.fallback.layer(inner),
            predicate: self.predicate.clone(),
        }
    }
}

// === impl MakeSvc ===

impl<A, B, P, T> tower::Service<T> for MakeSvc<A, B, P>
where
    A: tower::Service<T>,
    A::Error: Into<Error>,
    B: tower::Service<T> + Clone,
    B::Response: Into<A::Response>,
    B::Error: Into<Error>,
    P: Fn(&Error) -> bool + Clone,
    T: Clone,
{
    type Response = A::Response;
    type Error = Error;
    type Future = MakeFuture<A::Future, B, P, T>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.primary.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, target: T) -> Self::Future {
        MakeFuture {
            fallback: self.fallback.clone(),
            predicate: self.predicate.clone(),
            target: Some(target.clone()),
            state: FallbackState::Primary(self.primary.call(target)),
        }
    }
}

impl<A, B, P, T> Future for MakeFuture<A, B, P, T>
where
    A: Future,
    A::Error: Into<Error>,
    B: tower::Service<T>,
    B::Response: Into<A::Item>,
    B::Error: Into<Error>,
    P: Fn(&Error) -> bool,
{
    type Item = A::Item;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            self.state = match self.state {
                // We've called the primary service and are waiting for its
                // future to complete.
                FallbackState::Primary(ref mut f) => match f.poll() {
                    Ok(r) => return Ok(r),
                    Err(error) => {
                        let error = error.into();
                        if (self.predicate)(&error) {
                            trace!("{} matches; trying to fall back", error);
                            FallbackState::Waiting(self.target.take())
                        } else {
                            trace!("{} does not match; not falling back", error);
                            return Err(error);
                        }
                    }
                },
                // The primary service has returned an error matching the
                // predicate, and we are waiting for the fallback service to be ready.
                FallbackState::Waiting(ref mut target) => {
                    try_ready!(self.fallback.poll_ready().map_err(Into::into));
                    let target = target.take().expect("target should only be taken once");
                    FallbackState::Fallback(self.fallback.call(target))
                }
                // We've called the fallback service and are waiting for its
                // future to complete.
                FallbackState::Fallback(ref mut f) => {
                    return f.poll().map(|a| a.map(Into::into)).map_err(Into::into);
                }
            }
        }
    }
}
