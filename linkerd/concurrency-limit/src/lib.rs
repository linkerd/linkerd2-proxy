//! A layer that limits the number of in-flight requests to inner service.
//!
//! Modified from tower-concurrency-limit so that the Layer holds a semaphore
//! and, therefore, so that the limit applies across all services created by
//! this layer.

#![deny(warnings, rust_2018_idioms)]

use pin_project::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::{fmt, mem};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tower::Service;
use tracing::trace;

#[derive(Clone, Debug)]
pub struct Layer {
    semaphore: Arc<Semaphore>,
}

/// Enforces a limit on the number of concurrent requests to the inner service.
#[derive(Debug)]
pub struct ConcurrencyLimit<T> {
    inner: T,
    semaphore: Arc<Semaphore>,
    state: State,
}

enum State {
    Waiting(Pin<Box<dyn Future<Output = OwnedSemaphorePermit> + Send + 'static>>),
    Ready(OwnedSemaphorePermit),
    Empty,
}

/// Future for the `ConcurrencyLimit` service.
#[pin_project]
#[derive(Debug)]
pub struct ResponseFuture<T> {
    #[pin]
    inner: T,
    // We only keep this around so that it is dropped when the future completes.
    _permit: OwnedSemaphorePermit,
}

impl From<Arc<Semaphore>> for Layer {
    fn from(semaphore: Arc<Semaphore>) -> Self {
        Self { semaphore }
    }
}

impl Layer {
    pub fn new(max: usize) -> Self {
        Arc::new(Semaphore::new(max)).into()
    }
}

impl<S> tower::layer::Layer<S> for Layer {
    type Service = ConcurrencyLimit<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ConcurrencyLimit::new(inner, self.semaphore.clone())
    }
}

impl<T> ConcurrencyLimit<T> {
    /// Create a new concurrency limiter.
    pub fn new(inner: T, semaphore: Arc<Semaphore>) -> Self {
        ConcurrencyLimit {
            inner,
            semaphore,
            state: State::Empty,
        }
    }

    /// Get a reference to the inner service
    pub fn get_ref(&self) -> &T {
        &self.inner
    }

    /// Get a mutable reference to the inner service
    pub fn get_mut(&mut self) -> &mut T {
        &mut self.inner
    }

    /// Consume `self`, returning the inner service
    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<S, Request> Service<Request> for ConcurrencyLimit<S>
where
    S: Service<Request>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = ResponseFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        trace!(available = %self.semaphore.available_permits(), "acquiring permit");
        loop {
            self.state = match self.state {
                State::Ready(_) => {
                    trace!(available = %self.semaphore.available_permits(), "permit acquired");
                    return self.inner.poll_ready(cx);
                }
                State::Waiting(ref mut fut) => {
                    tokio::pin!(fut);
                    let permit = futures::ready!(fut.poll(cx));
                    State::Ready(permit)
                }
                State::Empty => State::Waiting(Box::pin(self.semaphore.clone().acquire_owned())),
            };
        }
    }

    fn call(&mut self, request: Request) -> Self::Future {
        // Make sure a permit has been acquired
        let _permit = match mem::replace(&mut self.state, State::Empty) {
            // Take the permit.
            State::Ready(permit) => permit,
            // whoopsie!
            _ => panic!("max requests in-flight; poll_ready must be called first"),
        };

        // Call the inner service
        let inner = self.inner.call(request);

        ResponseFuture { inner, _permit }
    }
}

impl<S> Clone for ConcurrencyLimit<S>
where
    S: Clone,
{
    fn clone(&self) -> ConcurrencyLimit<S> {
        ConcurrencyLimit {
            inner: self.inner.clone(),
            semaphore: self.semaphore.clone(),
            state: State::Empty,
        }
    }
}

impl<T> Future for ResponseFuture<T>
where
    T: Future,
{
    type Output = T::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().inner.poll(cx)
    }
}

impl fmt::Debug for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            State::Waiting(_) => f
                .debug_tuple("State::Waiting")
                .field(&format_args!("..."))
                .finish(),
            State::Ready(ref r) => f.debug_tuple("State::Ready").field(&r).finish(),
            State::Empty => f.debug_tuple("State::Empty").finish(),
        }
    }
}
