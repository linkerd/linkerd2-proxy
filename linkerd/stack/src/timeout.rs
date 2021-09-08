use crate::{Proxy, Service};
use linkerd_error::{Error, Result};
use pin_project::pin_project;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use thiserror::Error;
use tokio::time;

/// A timeout that wraps an underlying operation.
#[derive(Debug, Clone)]
pub struct Timeout<S> {
    inner: S,
    duration: Option<Duration>,
}

#[pin_project(project = TimeoutFutureProj)]
pub enum TimeoutFuture<F> {
    Passthru(#[pin] F),
    Timeout(#[pin] time::Timeout<F>, Duration),
}

/// An error representing that an operation timed out.
#[derive(Debug, Error)]
#[error("timeout after {:?}", self.0)]
pub struct TimeoutError(pub(crate) Duration);

// === impl Timeout ===

impl<S> Timeout<S> {
    /// Construct a new `Timeout` wrapping `inner`.
    pub fn new(inner: S, duration: Duration) -> Self {
        Self {
            inner,
            duration: Some(duration),
        }
    }

    pub fn passthru(inner: S) -> Self {
        Self {
            inner,
            duration: None,
        }
    }

    pub fn layer(duration: Duration) -> impl super::layer::Layer<S, Service = Self> + Clone {
        super::layer::mk(move |inner| Self::new(inner, duration))
    }
}

impl<P, S, Req> Proxy<Req, S> for Timeout<P>
where
    P: Proxy<Req, S>,
    S: Service<P::Request>,
{
    type Request = P::Request;
    type Response = P::Response;
    type Error = Error;
    type Future = TimeoutFuture<P::Future>;

    #[inline]
    fn proxy(&self, svc: &mut S, req: Req) -> Self::Future {
        let inner = self.inner.proxy(svc, req);
        match self.duration {
            None => TimeoutFuture::Passthru(inner),
            Some(t) => TimeoutFuture::Timeout(time::timeout(t, inner), t),
        }
    }
}

impl<S, Req> Service<Req> for Timeout<S>
where
    S: Service<Req>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = TimeoutFuture<S::Future>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<()>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    #[inline]
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
    type Output = Result<T>;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project() {
            TimeoutFutureProj::Passthru(f) => f.poll(cx).map_err(Into::into),
            TimeoutFutureProj::Timeout(f, duration) => {
                // If the `timeout` future failed, the error is always "elapsed";
                // errors from the underlying future will be in the success arm.
                let ready = futures::ready!(f.poll(cx)).map_err(|_| TimeoutError(*duration).into());
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

impl TimeoutError {
    /// Get the amount of time waited until this error was triggered.
    pub fn duration(&self) -> Duration {
        self.0
    }
}
