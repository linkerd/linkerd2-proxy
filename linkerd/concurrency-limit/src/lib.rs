//! A layer that limits the number of in-flight requests to inner service.
//!
//! Modified from tower-concurrency-limit so that the Layer holds a semaphore
//! and, therefore, so that the limit applies across all services created by
//! this layer.

#![deny(warnings, rust_2018_idioms)]

use futures::{try_ready, Future, Poll};
use linkerd2_error::Error;
use std::sync::Arc;
use tokio_sync::semaphore::{Permit, Semaphore};
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
    limit: Limit,
}

#[derive(Debug)]
struct Limit {
    semaphore: Arc<Semaphore>,
    permit: Permit,
}

/// Future for the `ConcurrencyLimit` service.
#[derive(Debug)]
pub struct ResponseFuture<T> {
    inner: T,
    semaphore: Arc<Semaphore>,
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
            limit: Limit {
                semaphore,
                permit: Permit::new(),
            },
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
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = ResponseFuture<S::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        trace!(available = %self.limit.semaphore.available_permits(), "acquiring permit");
        try_ready!(self
            .limit
            .permit
            .poll_acquire(&self.limit.semaphore)
            .map_err(Error::from));

        self.inner.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        // Make sure a permit has been acquired
        if self
            .limit
            .permit
            .try_acquire(&self.limit.semaphore)
            .is_err()
        {
            panic!("max requests in-flight; poll_ready must be called first");
        }

        // Call the inner service
        let inner = self.inner.call(request);

        // Forget the permit, the permit will be returned when
        // `future::ResponseFuture` is dropped.
        self.limit.permit.forget();

        ResponseFuture {
            inner,
            semaphore: self.limit.semaphore.clone(),
        }
    }
}

impl<S> Clone for ConcurrencyLimit<S>
where
    S: Clone,
{
    fn clone(&self) -> ConcurrencyLimit<S> {
        ConcurrencyLimit {
            inner: self.inner.clone(),
            limit: Limit {
                semaphore: self.limit.semaphore.clone(),
                permit: Permit::new(),
            },
        }
    }
}

impl Drop for Limit {
    fn drop(&mut self) {
        self.permit.release(&self.semaphore);
    }
}

impl<T> Future for ResponseFuture<T>
where
    T: Future,
    T::Error: Into<Error>,
{
    type Item = T::Item;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.inner.poll().map_err(Into::into)
    }
}

impl<T> Drop for ResponseFuture<T> {
    fn drop(&mut self) {
        trace!(available = %self.semaphore.available_permits() + 1, "releasing permit");
        self.semaphore.add_permits(1);
    }
}
