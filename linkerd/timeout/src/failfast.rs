//! A middleware that limits the amount of time the service may be not ready
//! before requests are failed.

use futures::TryFuture;
use linkerd2_error::Error;
use pin_project::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::time::{self, Sleep};
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
    Waiting(Sleep),
    FailFast,
}

#[pin_project(project = ResponseFutureProj)]
pub enum ResponseFuture<F> {
    Inner(#[pin] F),
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

impl<S> Clone for FailFast<S>
where
    S: Clone,
{
    fn clone(&self) -> Self {
        // When cloning failfast, we can't preserve the waiting state, so each
        // clone will have to detect its own failfast. Practically, this means
        // that each connection will have to wait for a timeout before
        // triggering failfast.
        Self {
            inner: self.inner.clone(),
            max_unavailable: self.max_unavailable.clone(),
            state: State::Open,
        }
    }
}

impl<S, T> tower::Service<T> for FailFast<S>
where
    S: tower::Service<T>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = ResponseFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match self.inner.poll_ready(cx) {
            // If the inner service is not ready, go into failfast after `max_unavailable`.
            Poll::Pending => loop {
                self.state = match self.state {
                    // The inner service just transitioned to NotReady, so initiate a new timeout.
                    State::Open => State::Waiting(time::sleep(self.max_unavailable)),

                    // A timeout has been set, so wait for it to complete.
                    State::Waiting(ref mut fut) => {
                        tokio::pin!(fut);
                        let poll = fut.poll(cx);
                        if poll.is_pending() {
                            trace!("Pending");
                            return Poll::Pending;
                        }
                        State::FailFast
                    }

                    // Admit requests and fail them immediately.
                    State::FailFast => {
                        debug!("Failing");
                        return Poll::Ready(Ok(()));
                    }
                };
            },

            // If the inner service is ready or has failed, then let subsequent
            // calls through to the service.
            ret => {
                match self.state {
                    State::Open => {}
                    State::Waiting(_) => trace!("Ready"),
                    State::FailFast => debug!("Recovered"),
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
    F: TryFuture,
    F::Error: Into<Error>,
{
    type Output = Result<F::Ok, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project() {
            ResponseFutureProj::Inner(f) => f.try_poll(cx).map_err(Into::into),
            ResponseFutureProj::FailFast => Poll::Ready(Err(FailFastError(()).into())),
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
    use std::time::Duration;
    use tokio_test::{assert_pending, assert_ready, assert_ready_ok};
    use tower::layer::Layer;
    use tower_test::mock::{self, Spawn};

    #[tokio::test]
    async fn fails_fast() {
        let max_unavailable = Duration::from_millis(100);
        let (service, mut handle) = mock::pair::<(), ()>();
        let mut service = Spawn::new(FailFastLayer::new(max_unavailable).layer(service));

        // The inner starts unavailable.
        handle.allow(0);
        assert_pending!(service.poll_ready());

        // Then we wait for the idle timeout, at which point the service
        // should start failing fast.
        tokio::time::sleep(max_unavailable + Duration::from_millis(1)).await;
        assert_ready_ok!(service.poll_ready());

        let err = service.call(()).await.err().expect("should failfast");
        assert!(err.is::<super::FailFastError>());

        // Then the inner service becomes available.
        handle.allow(1);
        assert_ready_ok!(service.poll_ready());
        let fut = service.call(());

        let ((), rsp) = handle.next_request().await.expect("must get a request");
        rsp.send_response(());

        let ret = fut.await;
        assert!(ret.is_ok());
    }
}
