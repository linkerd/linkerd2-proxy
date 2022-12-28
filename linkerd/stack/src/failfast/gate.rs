use super::Shared;
use futures::{ready, FutureExt};
use std::{
    sync::{atomic::Ordering, Arc},
    task::{Context, Poll},
};
use tokio_util::sync::ReusableBoxFuture;
use tracing::trace;

/// A middleware which, when paired with a [`FailFast`] middleware, advertises
/// the *actual* readiness state of the [`FailFast`]'s inner service up the
/// stack.
///
/// A [`FailFast`]/[`Gate`] pair is primarily intended to be used in
/// conjunction with a `tower::Buffer`. By placing the [`FailFast`] middleware
/// inside of the `Buffer` and the `Gate` middleware outside of the buffer,
/// the buffer's queue can be proactively drained when the inner service enters
/// failfast, while the outer `Gate` middleware will continue to return
/// [`Poll::Pending`] from its `poll_ready` method. This can be used to fail any
/// requests that have already been dispatched to the inner service while it is in
/// failfast, while allowing a load balancer or other traffic distributor to
/// send any new requests to a different backend until this backend actually
/// becomes available.
///
/// A `Layer`, such as a `Buffer` layer, may be wrapped in a new `Layer` which
/// produces a [`FailFast`]/[`Gate`] pair around the inner `Layer`'s
/// service using the [`FailFast::layer_gated`] function.
///
/// [`FailFast`]: super::FailFast
/// [`FailFast::layer_gated`]: super::FailFast::layer_gated
#[derive(Debug)]
pub struct Gate<S> {
    inner: S,
    shared: Arc<Shared>,

    /// Are we currently waiting on a notification that the inner service has
    /// exited failfast?
    is_waiting: bool,

    /// Future awaiting a notification from the inner `FailFast` service.
    waiting: ReusableBoxFuture<'static, ()>,
}

// === impl Gate ===

impl<S> Gate<S> {
    pub(super) fn new(inner: S, shared: Arc<Shared>) -> Self {
        Self {
            inner,
            shared,
            is_waiting: false,
            waiting: ReusableBoxFuture::new(async move { unreachable!() }),
        }
    }
}

impl<S> Clone for Gate<S>
where
    S: Clone,
{
    fn clone(&self) -> Self {
        Self::new(self.inner.clone(), self.shared.clone())
    }
}

impl<S, T> tower::Service<T> for Gate<S>
where
    S: tower::Service<T>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        loop {
            // Check the inner service's state. If it's not in failfast, use the
            // inner service directly.
            if !self.shared.in_failfast.load(Ordering::Acquire) {
                trace!("service is not in failfast");
                self.is_waiting = false;
                return self.inner.poll_ready(cx);
            }
            trace!("service in failfast");

            // Ensure that this task is notified when the inner service exits
            // failfast. Return pending until we are notified about a change.
            if !self.is_waiting {
                trace!("resetting watch");
                let shared = self.shared.clone();
                self.waiting.set(async move {
                    shared.notify.notified().await;
                    trace!("notified");
                });
                self.is_waiting = true;
            }
            trace!("waiting for service to become ready",);
            ready!(self.waiting.poll_unpin(cx));
            trace!("service became ready");
            self.is_waiting = false;
        }
    }

    #[inline]
    fn call(&mut self, req: T) -> Self::Future {
        self.inner.call(req)
    }
}
