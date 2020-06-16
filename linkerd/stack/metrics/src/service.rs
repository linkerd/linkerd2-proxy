use crate::Metrics;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct TrackService<S> {
    inner: S,
    metrics: Arc<Metrics>,
    blocked_since: Option<Instant>,
    // Helps determine when all instances are dropped.
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
                if self.blocked_since.is_none() {
                    self.blocked_since = Some(Instant::now());
                }
                Poll::Pending
            }
            Poll::Ready(Ok(())) => {
                self.metrics.ready_total.incr();
                if let Some(t0) = self.blocked_since.take() {
                    let not_ready = Instant::now() - t0;
                    self.metrics.poll_millis.add(not_ready.as_millis() as u64);
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => {
                self.metrics.error_total.incr();
                if let Some(t0) = self.blocked_since.take() {
                    let not_ready = Instant::now() - t0;
                    self.metrics.poll_millis.add(not_ready.as_millis() as u64);
                }
                Poll::Ready(Err(e))
            }
        }
    }

    fn call(&mut self, target: T) -> Self::Future {
        self.inner.call(target)
    }
}

impl<S> Drop for TrackService<S> {
    fn drop(&mut self) {
        if Arc::strong_count(&self._tracker) == 1 {
            self.metrics.drop_total.incr();
        }
    }
}
