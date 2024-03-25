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

    fn reset(sleep: Pin<&mut time::Sleep>, timeout: time::Duration) {
        // Roughly 30 years.
        const MAX: time::Duration = time::Duration::from_secs(86400 * 365 * 30);
        sleep.reset(time::Instant::now() + timeout.min(MAX));
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

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        let this = self.project();
        let poll = this.inner.poll_data(cx);
        match &poll {
            Poll::Pending if !*this.is_pending => {
                *this.is_pending = true;
                Self::reset(this.sleep.as_mut(), *this.timeout);
            }
            Poll::Ready(_) if *this.is_pending => {
                *this.is_pending = false;
            }
            _ => {}
        }
        poll.map_err(Into::into)
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<HeaderMap<HeaderValue>>, Self::Error>> {
        let this = self.project();
        let poll = this.inner.poll_trailers(cx);
        match &poll {
            Poll::Pending if !*this.is_pending => {
                *this.is_pending = true;
                Self::reset(this.sleep.as_mut(), *this.timeout);
            }
            Poll::Ready(_) if *this.is_pending => {
                *this.is_pending = false;
            }
            _ => {}
        }
        poll.map_err(Into::into)
    }

    fn poll_progress(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let this = self.project();

        match this.inner.poll_progress(cx).map_err(Into::into) {
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            poll if !*this.is_pending => return poll,
            _ => {}
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
