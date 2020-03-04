use crate::error::HumanDuration;
use futures::{future, Async, Future, Poll};
use linkerd2_error::Error;
use std::time::{Duration, Instant};
use tokio_timer::Delay;

#[derive(Copy, Clone, Debug)]
pub struct IdleLayer(Duration);

#[derive(Debug)]
pub struct Idle<S> {
    inner: Inner<S>,
    max_idle: Duration,
}

#[derive(Debug)]
enum Inner<S> {
    NotReady(S),
    Ready(Delay, S),
    Failed,
}

#[derive(Copy, Clone, Debug)]
pub struct IdleError(Duration);

// === impl IdleLayer ===

impl IdleLayer {
    pub fn new(max_idle: Duration) -> Self {
        IdleLayer(max_idle)
    }
}

impl<S> tower::layer::Layer<S> for IdleLayer {
    type Service = Idle<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Self::Service {
            max_idle: self.0,
            inner: Inner::NotReady(inner),
        }
    }
}

// === impl Idle ===

impl<S, T> tower::Service<T> for Idle<S>
where
    S: tower::Service<T>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::MapErr<S::Future, fn(S::Error) -> Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        loop {
            match std::mem::replace(&mut self.inner, Inner::Failed) {
                // If the inner service isn't ready, poll it until it is.
                Inner::NotReady(mut svc) => match svc.poll_ready().map_err(Into::into)? {
                    Async::NotReady => {
                        self.inner = Inner::NotReady(svc);
                        return Ok(Async::NotReady);
                    }
                    Async::Ready(()) => {
                        let timeout = Delay::new(Instant::now() + self.max_idle);
                        self.inner = Inner::Ready(timeout, svc);
                        // Continue, ensuring the timeout is polled.
                    }
                },

                // Once the inner service is ready, poll a timeout to be notified if it should be failed.
                Inner::Ready(mut timeout, svc) => {
                    if timeout.poll().expect("timer must not fail").is_not_ready() {
                        // The service stays ready until the timeout expires.
                        self.inner = Inner::Ready(timeout, svc);
                        return Ok(Async::Ready(()));
                    }
                    debug_assert!(match self.inner {
                        Inner::Failed => true,
                        _ => false,
                    });
                }

                // The inner service has been dropped this service will only return an IdleError now.
                Inner::Failed => return Err(IdleError(self.max_idle).into()),
            }
        }
    }

    fn call(&mut self, req: T) -> Self::Future {
        match std::mem::replace(&mut self.inner, Inner::Failed) {
            Inner::Ready(_, mut svc) => {
                let fut = svc.call(req);
                self.inner = Inner::NotReady(svc);
                fut.map_err(Into::into)
            }
            Inner::NotReady(_) | Inner::Failed => {
                panic!("poll_ready must be called");
            }
        }
    }
}

// === impl IdleError ===

impl std::fmt::Display for IdleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Service idled out after {}", HumanDuration(&self.0))
    }
}

impl std::error::Error for IdleError {}

#[cfg(test)]
mod test {
    use super::IdleLayer;
    use futures::{future, Future};
    use std::time::{Duration, Instant};
    use tower::layer::Layer;
    use tower::Service;
    use tower_test::mock;

    fn run<F, R>(f: F)
    where
        F: FnOnce() -> R + 'static,
        R: future::IntoFuture<Item = ()> + 'static,
    {
        tokio::runtime::current_thread::run(future::lazy(f).map_err(|_| panic!("Failed")));
    }

    #[test]
    fn call_succeeds_when_idle() {
        let max_idle = Duration::from_millis(100);
        let (service, mut handle) = mock::pair::<(), ()>();
        let mut service = IdleLayer::new(max_idle).layer(service);

        run(move || {
            // The inner starts available.
            handle.allow(1);
            assert!(service.poll_ready().unwrap().is_ready());

            // Then we wait for the idle timeout, at which point the service
            // should still be usable if we don't poll_ready again.
            tokio_timer::Delay::new(Instant::now() + max_idle + Duration::from_millis(1))
                .map_err(|_| ())
                .and_then(move |_| {
                    let fut = service.call(());
                    let ((), rsp) = handle.next_request().expect("must get a request");
                    rsp.send_response(());
                    fut.map_err(|_| ()).map(move |()| {
                        // Service remains usable.
                        assert!(service.poll_ready().unwrap().is_not_ready());
                        let _ = handle;
                    })
                })
        });
    }

    #[test]
    fn poll_ready_fails_after_idle() {
        let max_idle = Duration::from_millis(100);
        let (service, mut handle) = mock::pair::<(), ()>();
        let mut service = IdleLayer::new(max_idle).layer(service);

        run(move || {
            // The inner starts available.
            handle.allow(1);
            assert!(service.poll_ready().unwrap().is_ready());

            // Then we wait for the idle timeout, at which point the service
            // should fail.
            tokio_timer::Delay::new(Instant::now() + max_idle + Duration::from_millis(1))
                .map_err(|_| ())
                .map(move |_| {
                    assert!(service
                        .poll_ready()
                        .expect_err("must fail")
                        .is::<super::IdleError>());
                })
        });
    }
}
