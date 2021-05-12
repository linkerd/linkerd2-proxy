#![deny(warnings, rust_2018_idioms)]
#![allow(clippy::inconsistent_struct_constructor)]
use linkerd_error::Error;
use linkerd_stack::Proxy;
use pin_project::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use thiserror::Error;
use tokio::time;

mod failfast;

pub use self::failfast::{FailFast, FailFastError};

/// A timeout that wraps an underlying operation.
#[derive(Debug, Clone)]
pub struct Timeout<T> {
    inner: T,
    duration: Option<Duration>,
}

#[pin_project(project = TimeoutFutureProj)]
pub enum TimeoutFuture<F> {
    Passthru(#[pin] F),
    Timeout(#[pin] time::Timeout<F>, Duration),
}

/// An error representing that an operation timed out.
#[derive(Debug, Error)]
#[error("response timed out after {:?}", self.0)]
pub struct ResponseTimeout(pub(crate) Duration);

// === impl Timeout ===

impl<T> Timeout<T> {
    /// Construct a new `Timeout` wrapping `inner`.
    pub fn new(inner: T, duration: Duration) -> Self {
        Timeout {
            inner,
            duration: Some(duration),
        }
    }

    pub fn passthru(inner: T) -> Self {
        Timeout {
            inner,
            duration: None,
        }
    }
}

impl<P, S, Req> Proxy<Req, S> for Timeout<P>
where
    P: Proxy<Req, S>,
    S: tower::Service<P::Request>,
{
    type Request = P::Request;
    type Response = P::Response;
    type Error = Error;
    type Future = TimeoutFuture<P::Future>;

    fn proxy(&self, svc: &mut S, req: Req) -> Self::Future {
        let inner = self.inner.proxy(svc, req);
        match self.duration {
            None => TimeoutFuture::Passthru(inner),
            Some(t) => TimeoutFuture::Timeout(time::timeout(t, inner), t),
        }
    }
}

impl<S, Req> tower::Service<Req> for Timeout<S>
where
    S: tower::Service<Req>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = TimeoutFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let inner = self.inner.call(req);
        match self.duration {
            None => TimeoutFuture::Passthru(inner),
            Some(t) => TimeoutFuture::Timeout(time::timeout(t, inner), t),
        }
    }
}

impl<F, T, E> Future for TimeoutFuture<F>
where
    F: Future<Output = Result<T, E>>,
    E: Into<Error>,
{
    type Output = Result<T, Error>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project() {
            TimeoutFutureProj::Passthru(f) => f.poll(cx).map_err(Into::into),
            TimeoutFutureProj::Timeout(f, duration) => {
                // If the `timeout` future failed, the error is aways "elapsed";
                // errors from the underlying future will be in the success arm.
                let ready =
                    futures::ready!(f.poll(cx)).map_err(|_| ResponseTimeout(*duration).into());
                // If the inner future failed but the timeout was not elapsed,
                // then `ready` will be an `Ok(Err(e))`, so we need to convert
                // the inner error as well.
                let ready = ready.and_then(|x| x.map_err(Into::into));
                Poll::Ready(ready)
            }
        }
    }
}

// === impl ResponseTimeout ===

impl ResponseTimeout {
    /// Get the amount of time waited until this error was triggered.
    pub fn duration(&self) -> Duration {
        self.0
    }
}
