use crate::Metrics;
use std::{
    sync::Arc,
    task::{Context, Poll},
};
use tokio::time;

/// A service that tracks metrics about its readiness.
///
/// This service does NOT implement `Clone` because we want the drop count to
/// reflect actual drops.
#[derive(Debug)]
pub struct TrackService<S> {
    inner: S,
    metrics: Arc<Metrics>,
    blocked_since: Option<time::Instant>,
    // Metrics is shared across all distinct services, so we use a separate
    // tracker.
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
            _tracker: self._tracker.clone(),
            // The clone's block status is distinct.
            blocked_since: None,
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
                let now = time::Instant::now();
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
                    let not_ready = time::Instant::now().saturating_duration_since(t0);
                    self.metrics.poll_millis.add(not_ready.as_millis() as u64);
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => {
                self.metrics.error_total.incr();
                if let Some(t0) = self.blocked_since.take() {
                    let not_ready = time::Instant::now().saturating_duration_since(t0);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn clone_drop() {
        let metrics = Arc::new(Metrics::default());

        let (a, _a) = tower_test::mock::pair::<(), ()>();
        let a0 = TrackService::new(a, metrics.clone());
        let a1 = a0.clone();

        let (b, _b) = tower_test::mock::pair::<(), ()>();
        let b0 = TrackService::new(b, metrics.clone());

        drop(a1);
        assert_eq!(metrics.drop_total.value(), 0.0, "Not dropped yet");

        drop(b0);
        assert_eq!(
            metrics.drop_total.value(),
            1.0,
            "Dropping distinct service is counted"
        );

        drop(a0);
        assert_eq!(
            metrics.drop_total.value(),
            2.0,
            "Dropping last service clone counted"
        );

        assert_eq!(
            metrics.create_total.value(),
            0.0,
            "No creates by the service"
        );
    }

    #[cfg(test)]
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn clone_poll_ready() {
        let metrics = Arc::new(Metrics::default());
        let (mut svc0, mut handle) = tower_test::mock::spawn_with::<(), (), _, _>(|svc| {
            TrackService::new(svc, metrics.clone())
        });

        handle.allow(0);
        tokio_test::assert_pending!(svc0.poll_ready());
        let mut svc1 = svc0.clone();
        assert!(svc0.get_ref().blocked_since.is_some());
        assert!(svc1.get_ref().blocked_since.is_none());

        tokio_test::assert_pending!(svc1.poll_ready());
        assert!(svc0.get_ref().blocked_since.is_some());
        assert!(svc1.get_ref().blocked_since.is_some());

        time::sleep(time::Duration::from_secs(1)).await;
        handle.allow(2);
        tokio_test::assert_ready_ok!(svc0.poll_ready());
        tokio_test::assert_ready_ok!(svc1.poll_ready());
        assert!(svc0.get_ref().blocked_since.is_none());
        assert!(svc1.get_ref().blocked_since.is_none());

        assert_eq!(
            metrics.ready_total.value(),
            2.0,
            "Both clones should be counted discretely"
        );
        assert_eq!(
            metrics.not_ready_total.value(),
            2.0,
            "Both clones should be counted discretely"
        );
        assert_eq!(
            metrics.poll_millis.value(),
            2000.0,
            "Both clones should be counted discretely"
        );
    }
}
