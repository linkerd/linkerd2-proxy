//! A layer that limits the number of in-flight requests to inner service.
//!
//! Modified from tower-concurrency-limit so that the Layer holds a semaphore
//! and, therefore, so that the limit applies across all services created by
//! this layer.

#![deny(warnings, rust_2018_idioms)]
#![allow(clippy::inconsistent_struct_constructor)]

use linkerd_stack::layer;
use pin_project::pin_project;
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::sync::{OwnedSemaphorePermit as Permit, Semaphore};
use tokio_util::sync::PollSemaphore;
use tower::Service;
use tracing::trace;

/// Enforces a limit on the number of concurrent requests to the inner service.
#[derive(Debug)]
pub struct ConcurrencyLimit<T> {
    inner: T,
    semaphore: PollSemaphore,
    permit: Option<Permit>,
}

/// Future for the `ConcurrencyLimit` service.
#[pin_project]
#[derive(Debug)]
pub struct ResponseFuture<T> {
    #[pin]
    inner: T,
    // The permit is held until the future becomes ready.
    permit: Option<Permit>,
}

impl<S> ConcurrencyLimit<S> {
    /// Create a new concurrency-limiting layer.
    pub fn layer(limit: usize) -> impl layer::Layer<S, Service = Self> + Clone {
        let semaphore = Arc::new(Semaphore::new(limit));
        layer::mk(move |inner| Self::new(inner, semaphore.clone()))
    }

    fn new(inner: S, semaphore: Arc<Semaphore>) -> Self {
        ConcurrencyLimit {
            inner,
            semaphore: PollSemaphore::new(semaphore),
            permit: None,
        }
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
        trace!(
            // available = %self.semaphore.available_permits(),
            "acquiring permit"
        );
        loop {
            if self.permit.is_some() {
                trace!("permit already acquired; polling service");
                return self.inner.poll_ready(cx);
            }

            let permit =
                futures::ready!(self.semaphore.poll_acquire(cx)).expect("Semaphore must not close");
            self.permit = Some(permit);
            trace!("permit acquired");
        }
    }

    fn call(&mut self, request: Request) -> Self::Future {
        // Make sure a permit has been acquired
        let permit = self.permit.take();
        assert!(
            permit.is_some(),
            "max requests in-flight; poll_ready must be called first"
        );

        // Call the inner service
        let inner = self.inner.call(request);

        ResponseFuture { inner, permit }
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
            permit: None,
        }
    }
}

impl<T> Future for ResponseFuture<T>
where
    T: Future,
{
    type Output = T::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let res = futures::ready!(this.inner.poll(cx));
        let released = this.permit.take().is_some();
        debug_assert!(
            released,
            "Permit must be released when the future completes"
        );
        trace!("permit released");
        Poll::Ready(res)
    }
}
