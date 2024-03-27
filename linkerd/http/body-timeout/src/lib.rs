#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use http::{HeaderMap, HeaderValue};
use http_body::Body;
use linkerd_error::Error;
use pin_project::pin_project;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::time;

/// A [`Body`] that times out if it does not yield data within the given duration.
#[derive(Debug)]
#[pin_project]
pub struct TimeoutBody<B> {
    #[pin]
    inner: B,
    sleep: Pin<Box<time::Sleep>>,
    timeout: time::Duration,
    is_pending: bool,
}

#[derive(Debug, thiserror::Error)]
#[error("body timeout after {0:?}")]
pub struct BodyTimeoutError(time::Duration);

// === impl TimeoutBody ===

impl<B> TimeoutBody<B> {
    pub fn new(timeout: time::Duration, inner: B) -> Self
    where
        B: Body + Send + 'static,
        B::Data: Send + 'static,
        B::Error: Into<Error>,
    {
        Self {
            inner,
            timeout,
            is_pending: false,
            sleep: Box::pin(time::sleep(time::Duration::MAX)),
        }
    }
}

impl<B> Body for TimeoutBody<B>
where
    B: Body + Send + 'static,
    B::Data: Send + 'static,
    B::Error: Into<Error>,
{
    type Data = B::Data;
    type Error = Error;

    #[inline]
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    #[inline]
    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        let this = self.project();
        *this.is_pending = false;
        this.inner.poll_data(cx).map_err(Into::into)
    }

    #[inline]
    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<HeaderMap<HeaderValue>>, Self::Error>> {
        let this = self.project();
        *this.is_pending = false;
        this.inner.poll_trailers(cx).map_err(Into::into)
    }

    fn poll_progress(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let this = self.project();

        let _ = this.inner.poll_progress(cx).map_err(Into::into)?;

        if !*this.is_pending {
            // Avoid overflows by capping MAX to roughly 30 years.
            const MAX: time::Duration = time::Duration::from_secs(86400 * 365 * 30);
            this.sleep
                .as_mut()
                .reset(time::Instant::now() + (*this.timeout).min(MAX));
            *this.is_pending = true;
        }

        match this.sleep.as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(Err(BodyTimeoutError(*this.timeout).into())),
            Poll::Pending => Poll::Pending,
        }
    }

    #[inline]
    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}
