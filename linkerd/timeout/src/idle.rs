use crate::error::HumanDuration;
use futures::{future, TryFutureExt};
use linkerd2_error::Error;
use std::future::Future;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::time::{self, Delay, Instant};

#[derive(Copy, Clone, Debug)]
pub struct IdleLayer(Duration);

#[derive(Debug)]
pub struct Idle<S> {
    inner: S,
    idle: Delay,
    timeout: Duration,
}

#[derive(Copy, Clone, Debug)]
pub struct IdleError(Duration);

// === impl IdleLayer ===

impl IdleLayer {
    pub fn new(timeout: Duration) -> Self {
        IdleLayer(timeout)
    }
}

impl<S> tower::layer::Layer<S> for IdleLayer {
    type Service = Idle<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Self::Service {
            inner,
            timeout: self.0,
            idle: time::delay_for(self.0),
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

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let idle = &mut self.idle;
        tokio::pin!(idle);
        if idle.poll(cx).is_ready() {
            return Poll::Ready(Err(IdleError(self.timeout).into()));
        }

        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, req: T) -> Self::Future {
        self.idle.reset(Instant::now() + self.timeout);
        self.inner.call(req).map_err(Into::into)
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
    use std::time::Duration;
    use tokio_test::{assert_pending, assert_ready, assert_ready_ok};
    use tower::layer::Layer;
    use tower_test::mock::{self, Spawn};

    #[tokio::test]
    async fn call_succeeds_when_idle() {
        let timeout = Duration::from_millis(100);
        let (service, mut handle) = mock::pair::<(), ()>();
        let mut service = Spawn::new(IdleLayer::new(timeout).layer(service));

        // The inner starts available.
        handle.allow(1);
        assert_ready_ok!(service.poll_ready());

        // Then we wait for the idle timeout, at which point the service
        // should still be usable if we don't poll_ready again.
        tokio::time::delay_for(timeout + Duration::from_millis(1)).await;

        let fut = service.call(());
        let ((), rsp) = handle.next_request().await.expect("must get a request");
        rsp.send_response(());
        // Service remains usable.
        fut.await.expect("call");

        assert_pending!(service.poll_ready());
    }

    #[tokio::test]
    async fn poll_ready_fails_after_idle() {
        let timeout = Duration::from_millis(100);
        let (service, mut handle) = mock::pair::<(), ()>();
        let mut service = Spawn::new(IdleLayer::new(timeout).layer(service));
        // The inner starts available.
        handle.allow(1);
        assert_ready_ok!(service.poll_ready());

        // Then we wait for the idle timeout, at which point the service
        // should fail.
        tokio::time::delay_for(timeout + Duration::from_millis(1)).await;
        assert!(assert_ready!(service.poll_ready())
            .unwrap_err()
            .is::<super::IdleError>());
    }
}
