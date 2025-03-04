use super::metrics::BodyDataMetrics;
use http_body::{Frame, SizeHint};
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
    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.project();
        let inner = this.inner;
        let BodyDataMetrics { frame_size } = this.metrics;

        let frame = std::task::ready!(inner.poll_frame(cx));

        if let Some(Ok(frame)) = &frame {
            if let Some(data) = frame.data_ref() {
                // We've polled and yielded a new chunk! Increment our telemetry.
                //
                // NB: We're careful to call `remaining()` rather than `chunk()`, which
                // "can return a shorter slice (this allows non-continuous internal representation)."
                let bytes = bytes::Buf::remaining(data);
                frame_size.observe(linkerd_metrics::to_f64(bytes as u64));
            }
        }

        Poll::Ready(frame)
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
