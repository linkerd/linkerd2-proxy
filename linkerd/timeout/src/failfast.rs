//! A middleware that limits the amount of time the service may be not ready
//! before requests are failed.

use super::timer::Delay;
use futures::{Async, Future, Poll};
use linkerd2_error::Error;
use std::time::{Duration, Instant};
use tracing::{debug, trace};

#[derive(Copy, Clone, Debug)]
pub struct FailFastLayer(Duration);

#[derive(Debug)]
pub struct FailFast<S> {
    inner: S,
    max_unavailable: Duration,
    state: State,
}

/// An error representing that an operation timed out.
#[derive(Debug)]
pub struct FailFastError(());

#[derive(Debug)]
enum State {
    Open,
    Waiting(Delay),
    FailFast,
}

pub enum ResponseFuture<F> {
    Inner(F),
    FailFast,
}

// === impl FailFastLayer ===

impl FailFastLayer {
    pub fn new(max_unavailable: Duration) -> Self {
        FailFastLayer(max_unavailable)
    }
}

impl<S> tower::layer::Layer<S> for FailFastLayer {
    type Service = FailFast<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Self::Service {
            inner,
            max_unavailable: self.0,
            state: State::Open,
        }
    }
}

// === impl FailFast ===

impl<S, T> tower::Service<T> for FailFast<S>
where
    S: tower::Service<T>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = ResponseFuture<S::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        match self.inner.poll_ready() {
            // If the inner service is not ready, go into failfast after `max_unavailable`.
            Ok(Async::NotReady) => loop {
                self.state = match self.state {
                    // The inner service just transitioned to NotReady, so initiate a new timeout.
                    State::Open => {
                        State::Waiting(Delay::new(Instant::now() + self.max_unavailable))
                    }

                    // A timeout has been set, so wait for it to complete.
                    State::Waiting(ref mut fut) => {
                        let poll = fut.poll();
                        debug_assert!(poll.is_ok(), "timer must not fail");
                        if let Ok(Async::NotReady) = poll {
                            trace!("Pending");
                            return Ok(Async::NotReady);
                        }
                        State::FailFast
                    }

                    // Admit requests and fail them immediately.
                    State::FailFast => {
                        debug!("Failing");
                        return Ok(Async::Ready(()));
                    }
                };
            },

            // If the inner service is ready or has failed, then let subsequent
            // calls through to the service.
            ret => {
                match self.state {
                    State::Open => {}
                    _ => debug!("Recovered"),
                }
                self.state = State::Open;
                ret.map_err(Into::into)
            }
        }
    }

    fn call(&mut self, req: T) -> Self::Future {
        match self.state {
            State::Open => ResponseFuture::Inner(self.inner.call(req)),
            State::FailFast => ResponseFuture::FailFast,
            State::Waiting(_) => panic!("poll_ready must be called"),
        }
    }
}

impl<F> Future for ResponseFuture<F>
where
    F: Future,
    F::Error: Into<Error>,
{
    type Item = F::Item;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self {
            ResponseFuture::Inner(ref mut f) => f.poll().map_err(Into::into),
            ResponseFuture::FailFast => Err(FailFastError(()).into()),
        }
    }
}

// === impl FailFastError ===

impl std::fmt::Display for FailFastError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Service in fail-fast")
    }
}

impl std::error::Error for FailFastError {}

#[cfg(test)]
mod test {
    use super::FailFastLayer;
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
    fn fails_fast() {
        let max_unavailable = Duration::from_millis(100);
        let (service, mut handle) = mock::pair::<(), ()>();
        let mut service = FailFastLayer::new(max_unavailable).layer(service);

        run(move || {
            // The inner starts unavailable.
            handle.allow(0);
            assert!(service.poll_ready().unwrap().is_not_ready());

            // Then we wait for the idle timeout, at which point the service
            // should start failing fast.
            tokio_timer::Delay::new(Instant::now() + max_unavailable + Duration::from_millis(1))
                .map_err(|_| ())
                .and_then(move |_| {
                    assert!(service.poll_ready().unwrap().is_ready());
                    service.call(()).then(move |ret| {
                        let err = ret.err().expect("should failfast");
                        assert!(err.is::<super::FailFastError>());

                        // Then the inner service becomes available.
                        handle.allow(1);
                        assert!(service.poll_ready().unwrap().is_ready());
                        let fut = service.call(());

                        let ((), rsp) = handle.next_request().expect("must get a request");
                        rsp.send_response(());

                        fut.then(|ret| {
                            assert!(ret.is_ok());
                            Ok(())
                        })
                    })
                })
        });
    }
}
