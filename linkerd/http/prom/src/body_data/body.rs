use super::metrics::BodyDataMetrics;
use http::HeaderMap;
use http_body::SizeHint;
use pin_project::pin_project;
use std::{
    pin::Pin,
    task::{Context, Poll},
};

/// An instrumented body.
#[pin_project]
pub struct Body<B> {
    /// The inner body.
    #[pin]
    inner: B,
    /// Metrics with which the inner body will be instrumented.
    metrics: BodyDataMetrics,
}

impl<B> Body<B> {
    /// Returns a new, instrumented body.
    pub(crate) fn new(body: B, metrics: BodyDataMetrics) -> Self {
        Self {
            inner: body,
            metrics,
        }
    }
}

impl<B> http_body::Body for Body<B>
where
    B: http_body::Body,
{
    type Data = B::Data;
    type Error = B::Error;

    /// Attempt to pull out the next data buffer of this stream.
    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        let this = self.project();
        let inner = this.inner;
        let BodyDataMetrics {
            frames_total,
            frames_bytes,
        } = this.metrics;

        let data = std::task::ready!(inner.poll_data(cx));

        if let Some(Ok(data)) = data.as_ref() {
            // We've polled and yielded a new chunk! Increment our telemetry.
            //
            // NB: We're careful to call `remaining()` rather than `chunk()`, which
            // "can return a shorter slice (this allows non-continuous internal representation)."
            let bytes = <B::Data as bytes::Buf>::remaining(data)
                .try_into()
                .unwrap_or(u64::MAX);
            frames_bytes.inc_by(bytes);
            frames_total.inc();
        }

        Poll::Ready(data)
    }

    #[inline]
    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<HeaderMap>, Self::Error>> {
        self.project().inner.poll_trailers(cx)
    }

    #[inline]
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    #[inline]
    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}
