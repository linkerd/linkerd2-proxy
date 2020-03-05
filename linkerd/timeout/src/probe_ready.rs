use futures::{Future, Poll};
use linkerd2_error::Error;
use std::time::{Duration, Instant};
use tokio_timer::Delay;

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
            probe: Delay::new(Instant::now()),
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

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        let ready = self.inner.poll_ready()?;
        self.probe.reset(Instant::now() + self.interval);
        self.probe.poll().expect("timer must succeed");
        Ok(ready)
    }

    fn call(&mut self, req: T) -> Self::Future {
        self.inner.call(req)
    }
}

#[cfg(test)]
mod test {
    use super::ProbeReadyLayer;
    use futures::{future, Async, Future, Poll};
    use linkerd2_error::Never;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Weak,
    };
    use std::time::{Duration, Instant};
    use tower::layer::Layer;
    use tower::Service;

    fn run<F, R>(f: F)
    where
        F: FnOnce() -> R + 'static,
        R: future::IntoFuture<Item = ()> + 'static,
    {
        tokio::runtime::current_thread::run(future::lazy(f).map_err(|_| panic!("Failed")));
    }

    #[test]
    fn probes() {
        struct Ready;
        impl tower::Service<()> for Ready {
            type Response = ();
            type Error = Never;
            type Future = future::FutureResult<Self::Response, Self::Error>;

            fn poll_ready(&mut self) -> Poll<(), Self::Error> {
                Ok(Async::Ready(()))
            }

            fn call(&mut self, _: ()) -> Self::Future {
                future::ok(())
            }
        }

        struct Drive(super::ProbeReady<Ready>, Weak<AtomicUsize>);
        impl Future for Drive {
            type Item = ();
            type Error = ();
            fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
                if let Some(c) = self.1.upgrade() {
                    self.0.poll_ready().map_err(|_| ())?;
                    c.fetch_add(1, Ordering::SeqCst);
                    return Ok(Async::NotReady);
                }

                Ok(Async::Ready(()))
            }
        }

        run(move || {
            let interval = Duration::from_millis(100);
            let count = Arc::new(AtomicUsize::new(0));
            let service = ProbeReadyLayer::new(interval).layer(Ready);
            tokio::spawn(Drive(service, Arc::downgrade(&count)));
            let delay = (2 * interval) + Duration::from_millis(3);
            tokio_timer::Delay::new(Instant::now() + delay)
                .map_err(|_| ())
                .map(move |_| {
                    let polls = count.load(Ordering::SeqCst);
                    assert!(polls >= 3, "{}", polls);
                })
        });
    }
}
