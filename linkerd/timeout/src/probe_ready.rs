use linkerd2_error::Error;
use std::future::Future;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::time::{self, Delay, Instant};

#[derive(Copy, Clone, Debug)]
pub struct ProbeReadyLayer(Duration);

/// Ensures that the inner service is polled at least once per `interval`.
#[derive(Debug)]
pub struct ProbeReady<S> {
    inner: S,
    probe: Delay,
    interval: Duration,
}

// === impl ProbeReadyLayer ===

impl ProbeReadyLayer {
    pub fn new(interval: Duration) -> Self {
        ProbeReadyLayer(interval)
    }
}

impl<S> tower::layer::Layer<S> for ProbeReadyLayer {
    type Service = ProbeReady<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Self::Service {
            inner,
            interval: self.0,
            probe: time::delay_until(Instant::now()),
        }
    }
}

// === impl ProbeReady ===

impl<S, T> tower::Service<T> for ProbeReady<S>
where
    S: tower::Service<T>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let ready = self.inner.poll_ready(cx)?;
        self.probe.reset(Instant::now() + self.interval);
        let probe = &mut self.probe;
        tokio::pin!(probe);
        // We don't care if the timer is pending.
        let _ = probe.poll(cx);
        ready.map(Ok)
    }

    fn call(&mut self, req: T) -> Self::Future {
        self.inner.call(req)
    }
}

#[cfg(test)]
mod test {
    use super::ProbeReadyLayer;
    use futures::future;
    use linkerd2_error::Never;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::task::{Context, Poll};
    use std::time::Duration;
    use tower::layer::Layer;
    use tower::Service;

    #[tokio::test]
    async fn probes() {
        struct Ready;
        impl tower::Service<()> for Ready {
            type Response = ();
            type Error = Never;
            type Future =
                Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

            fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
                Poll::Ready(Ok(()))
            }

            fn call(&mut self, _: ()) -> Self::Future {
                Box::pin(async { Ok(()) })
            }
        }

        let interval = Duration::from_millis(100);
        let count = Arc::new(AtomicUsize::new(0));
        let service = ProbeReadyLayer::new(interval).layer(Ready);
        let count2 = Arc::downgrade(&count);
        tokio::spawn(async move {
            let mut service = service;
            let count = count2;
            future::poll_fn(|cx| {
                if let Some(c) = count.upgrade() {
                    let _ = service.poll_ready(cx).map_err(|_| ())?;
                    c.fetch_add(1, Ordering::SeqCst);
                    return Poll::Pending;
                }

                Poll::Ready(Ok::<(), ()>(()))
            })
            .await
        });

        let delay = (2 * interval) + Duration::from_millis(3);
        tokio::time::delay_for(delay).await;

        let polls = count.load(Ordering::SeqCst);
        assert!(polls >= 3, "{}", polls);
    }
}
