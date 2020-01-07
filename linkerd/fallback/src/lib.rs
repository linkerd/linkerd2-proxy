#![deny(warnings, rust_2018_idioms)]

use futures::{Async, Future, Poll};
use linkerd2_error::Error;
use tower::util::{Oneshot, ServiceExt};
use tracing::{debug, trace};

/// A fallback layer composing two service builders.
///
/// If the future returned by the primary builder's `MakeService` fails with
/// an error matching a given predicate, the fallback future will attempt
/// to call the secondary `MakeService`.
#[derive(Clone, Debug)]
pub struct Layer<F, P = fn(&Error) -> bool> {
    fallback: F,
    predicate: P,
}

#[derive(Clone, Debug)]
pub struct Fallback<I, F, P> {
    inner: I,
    fallback: F,
    predicate: P,
}

pub struct MakeFuture<I, F, P> {
    predicate: P,
    state: State<I, F>,
}

enum State<I, F> {
    Primary(I, Option<F>),
    Fallback(F),
}

pub fn layer<F>(fallback: F) -> Layer<F> {
    let predicate: fn(&Error) -> bool = |_| true;
    Layer {
        fallback,
        predicate,
    }
}

// === impl Layer ===

impl<F> Layer<F> {
    /// Returns a `Layer` that uses the given `predicate` to determine whether
    /// to fall back.
    pub fn with_predicate<P>(self, predicate: P) -> Layer<F, P>
    where
        P: Fn(&Error) -> bool + Clone,
    {
        Layer {
            fallback: self.fallback,
            predicate,
        }
    }

    /// Returns a `Layer` that falls back if the error or its source is of
    /// type `E`.
    pub fn on_error<E>(self) -> Layer<F>
    where
        E: std::error::Error + 'static,
    {
        self.with_predicate(|e| e.is::<E>() || e.source().map(|s| s.is::<E>()).unwrap_or(false))
    }
}

impl<I, F, P> tower::layer::Layer<I> for Layer<F, P>
where
    F: Clone,
    P: Clone,
{
    type Service = Fallback<I, F, P>;

    fn layer(&self, inner: I) -> Self::Service {
        Self::Service {
            inner,
            fallback: self.fallback.clone(),
            predicate: self.predicate.clone(),
        }
    }
}

// === impl Fallback ===

impl<I, F, P, T> tower::Service<T> for Fallback<I, F, P>
where
    T: Clone,
    I: tower::Service<T>,
    I::Error: Into<Error>,
    F: tower::Service<T> + Clone,
    F::Response: Into<I::Response>,
    F::Error: Into<Error>,
    P: Fn(&Error) -> bool,
    P: Clone,
{
    type Response = I::Response;
    type Error = Error;
    type Future = MakeFuture<I::Future, Oneshot<F, T>, P>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, target: T) -> Self::Future {
        let future = self.inner.call(target.clone());
        Self::Future {
            state: State::Primary(future, Some(self.fallback.clone().oneshot(target))),
            predicate: self.predicate.clone(),
        }
    }
}

impl<I, F, P> Future for MakeFuture<I, F, P>
where
    I: Future,
    I::Error: Into<Error>,
    F: Future,
    F::Item: Into<I::Item>,
    F::Error: Into<Error>,
    P: Fn(&Error) -> bool,
{
    type Item = I::Item;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            self.state = match self.state {
                State::Primary(ref mut i, ref mut f) => match i.poll() {
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Ok(Async::Ready(v)) => {
                        trace!("primary succeeded");
                        return Ok(Async::Ready(v));
                    }
                    Err(e) => {
                        let error = e.into();
                        if !(self.predicate)(&error) {
                            return Err(error);
                        }

                        debug!(%error, "falling back");
                        State::Fallback(f.take().unwrap())
                    }
                },
                State::Fallback(ref mut f) => {
                    return f.poll().map(|a| a.map(Into::into)).map_err(Into::into);
                }
            }
        }
    }
}
