use crate::Metrics;
use std::{
    sync::Arc,
    task::{Context, Poll},
};
use tokio::time::Instant;

/// A service that tracks metrics about its readiness.
///
/// This service does NOT implement `Clone` because we want the drop count to
/// reflect actual drops.
#[derive(Debug)]
pub struct TrackService<S> {
    inner: S,
    metrics: Arc<Metrics>,
    blocked_since: Option<Instant>,
    _tracker: Arc<()>,
}

impl<S> TrackService<S> {
    pub(crate) fn new(inner: S, metrics: Arc<Metrics>) -> Self {
        Self {
            inner,
            metrics,
            blocked_since: None,
            _tracker: Arc::new(()),
        }
    }
}

impl<S: Clone> Clone for TrackService<S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            metrics: self.metrics.clone(),
            blocked_since: None,
            _tracker: self._tracker.clone(),
        }
    }
}

impl<T, S> tower::Service<T> for TrackService<S>
where
    S: tower::Service<T>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match self.inner.poll_ready(cx) {
            Poll::Pending => {
                self.metrics.not_ready_total.incr();
                // If the service was already pending, then add the time we
                // waited and reset blocked_since. This allows the value to be
                // updated even when we're "stuck" in pending.
                let now = Instant::now();
                if let Some(t0) = self.blocked_since.take() {
                    let not_ready = now.saturating_duration_since(t0);
                    self.metrics.poll_millis.add(not_ready.as_millis() as u64);
                }
                self.blocked_since = Some(now);
                Poll::Pending
            }
            Poll::Ready(Ok(())) => {
                self.metrics.ready_total.incr();
                if let Some(t0) = self.blocked_since.take() {
                    let not_ready = Instant::now().saturating_duration_since(t0);
                    self.metrics.poll_millis.add(not_ready.as_millis() as u64);
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => {
                self.metrics.error_total.incr();
                if let Some(t0) = self.blocked_since.take() {
                    let not_ready = Instant::now().saturating_duration_since(t0);
                    self.metrics.poll_millis.add(not_ready.as_millis() as u64);
                }
                Poll::Ready(Err(e))
            }
        }
    }

    #[inline]
    fn call(&mut self, target: T) -> Self::Future {
        self.inner.call(target)
    }
}

impl<S> Drop for TrackService<S> {
    fn drop(&mut self) {
        if Arc::strong_count(&self._tracker) == 1 {
            // If we're the last reference to the metrics, then we can
            // increment the drop count.
            self.metrics.drop_total.incr();
        }
    }
}
