use super::layer;
use futures::{future, TryFutureExt};
use linkerd_error::Error;
use std::task::{Context, Poll};
use thiserror::Error;
use tower::Service;

/// A middleware that sheds load when the inner `Service` isn't ready.
#[derive(Debug)]
pub struct LoadShed<S> {
    inner: S,
    open: bool,
}

/// An error representing that a service is shedding load.
#[derive(Debug, Error)]
#[error("service unavailable")]
pub struct LoadShedError(());

// === impl LoadShed ===

impl<S> LoadShed<S> {
    /// Returns a `Layer` that fails requests when the inner service is not
    /// ready.
    ///
    /// Innner services MUST be responsible for driving themselves to ready
    /// (e.g., via [`SpawnReady`], where appropriate).
    ///
    /// [`SpawnReady`]: tower::spawn_ready::SpawnReady
    pub fn layer() -> impl layer::Layer<S, Service = Self> + Copy + Clone {
        layer::mk(Self::new)
    }

    /// Fails requests when the inner service is not
    /// ready.
    ///
    /// Innner services MUST be responsible for driving themselves to ready
    /// (e.g., via [`SpawnReady`], where appropriate).
    ///
    /// [`SpawnReady`]: tower::spawn_ready::SpawnReady
    pub fn new(inner: S) -> Self {
        Self { inner, open: true }
    }
}

impl<S: Clone> Clone for LoadShed<S> {
    fn clone(&self) -> Self {
        Self::new(self.inner.clone())
    }
}

impl<S, Req> Service<Req> for LoadShed<S>
where
    S: Service<Req>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::Either<
        future::MapErr<S::Future, fn(S::Error) -> Error>,
        future::Ready<Result<S::Response, Error>>,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match self.inner.poll_ready(cx) {
            Poll::Ready(ready) => {
                if !self.open {
                    tracing::debug!("Service has become available");
                    self.open = true;
                }
                Poll::Ready(ready.map_err(Into::into))
            }

            // If the inner service is not ready, we return ready anyway so load
            // can be shed by failing requests. This inner service MUST be
            // responsible for driving itself to ready.
            Poll::Pending => {
                if self.open {
                    tracing::debug!("Service has become unavailable");
                    self.open = false;
                }
                Poll::Ready(Ok(()))
            }
        }
    }

    fn call(&mut self, req: Req) -> Self::Future {
        if self.open {
            future::Either::Left(self.inner.call(req).map_err(Into::into))
        } else {
            tracing::debug!("Service shedding load");
            future::Either::Right(future::err(LoadShedError(()).into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_test::{assert_ready_err, assert_ready_ok};
    use tower_test::mock::{self, Spawn};

    #[tokio::test]
    async fn sheds_load() {
        let _trace = linkerd_tracing::test::trace_init();
        let (service, mut handle) = mock::pair::<(), ()>();
        let mut service = Spawn::new(LoadShed::new(service));

        // The inner service accepts one request.
        handle.allow(1);
        assert_ready_ok!(service.poll_ready());

        let call = service.call(());
        let (_, send_rsp) = handle
            .next_request()
            .await
            .expect("should call inner service");
        send_rsp.send_response(());
        call.await.expect("should succeed");

        // Then the inner service becomes unavailable.
        assert_ready_ok!(
            service.poll_ready(),
            "load shed service should always be ready"
        );
        service.call(()).await.expect_err("should shed load");

        // The inner service becomes ready again, so it should no longer shed load.
        handle.allow(1);
        assert_ready_ok!(service.poll_ready());

        let call = service.call(());
        let (_, send_rsp) = handle
            .next_request()
            .await
            .expect("should call inner service");
        send_rsp.send_response(());
        call.await.expect("should succeed");
    }

    #[tokio::test]
    async fn propagates_inner_error() {
        let _trace = linkerd_tracing::test::trace_init();
        let (service, mut handle) = mock::pair::<(), ()>();
        let mut service = Spawn::new(LoadShed::new(service));

        // The inner service errors.
        handle.send_error("service machine broke");
        assert_ready_err!(service.poll_ready());
    }
}
