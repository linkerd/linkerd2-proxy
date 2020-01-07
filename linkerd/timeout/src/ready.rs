use super::Error;
use futures::{future, Async, Future, Poll};
use std::time::Duration;
use tokio_timer::{clock, Delay};
use tower::Service;
use tracing::{debug, trace};

#[derive(Copy, Clone, Debug)]
pub struct Layer(Duration);

/// A timeout that wraps an underlying operation.
#[derive(Debug)]
pub struct TimeoutReady<S> {
    inner: S,
    duration: Duration,
    timeout: Option<Delay>,
}

// === impl Layrer ===

impl Layer {
    pub fn new(duration: Duration) -> Self {
        Layer(duration)
    }
}

impl<S> tower::layer::Layer<S> for Layer {
    type Service = TimeoutReady<S>;

    fn layer(&self, inner: S) -> Self::Service {
        TimeoutReady::new(inner, self.0)
    }
}

// === impl Timeout ===

impl<T: Clone> Clone for TimeoutReady<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            duration: self.duration,
            timeout: None,
        }
    }
}

impl<T> TimeoutReady<T> {
    pub fn new(inner: T, duration: Duration) -> Self {
        Self {
            inner,
            duration,
            timeout: None,
        }
    }
}

impl<S, Req> Service<Req> for TimeoutReady<S>
where
    S: Service<Req>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::MapErr<S::Future, fn(S::Error) -> Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        if self.inner.poll_ready().map_err(Into::into)?.is_ready() {
            trace!("service became ready");
            self.timeout = None;
            return Ok(Async::Ready(()));
        }

        if self.timeout.is_none() {
            trace!(timeout = ?self.duration, "service is not ready");
            self.timeout = Some(Delay::new(clock::now() + self.duration));
        }
        let timeout = self.timeout.as_mut().unwrap();
        if timeout.poll().map_err(Into::<Error>::into)?.is_ready() {
            debug!(timeout = ?self.duration, "service acquisition failed");
            return Err(super::error::ReadyTimeout(self.duration).into());
        }

        Ok(Async::NotReady)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        self.inner.call(req).map_err(Into::into)
    }
}
