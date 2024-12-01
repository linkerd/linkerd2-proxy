//! Asynchronous request or response body types.

use pin_project::pin_project;
use std::{
    pin::Pin,
    task::{ready, Context, Poll},
};

/// Reëxports the legacy interfaces of the [`http-body`] crate.
pub mod legacy {
    pub use http_0_2 as http;
    pub use http_body_0_4 as http_body;
}

/// Reëxports the v1.0 interfaces of the [`http-body`] crate.
pub mod v1_0 {
    pub use http_1_2 as http;
    pub use http_body_1_0 as http_body;
}

/// Compatibility bridges between new and legacy crates.
mod compat {
    use {crate::legacy::http as http_legacy, crate::v1_0::http as http_v1};

    /// Returns an upgraded [`HeaderMap`][http_legacy::HeaderMap].
    pub(super) fn headers(mut legacy: http_legacy::HeaderMap) -> http_v1::HeaderMap {
        let mut out = http_v1::HeaderMap::with_capacity(legacy.len());

        // Iterate through the header map.
        //
        // NB: A `None` header name means that this is a value associated with the last previously
        // seen header name. Because we must hang onto this value, cloning it when appending to
        // the next entry, mark this as allowing for "unused" assignments.
        #[allow(unused_assignments)]
        let mut name = None;
        for (mut curr, value) in legacy.drain() {
            // Cache this as our last seen name, if this item has a name associated.
            name = curr.take().map(header_name);
            let value = header_value(value);
            // Append this name/value pair to our upgraded header map.
            out.append(name.clone().unwrap(), value);
        }

        out
    }

    fn header_name(legacy: http_legacy::HeaderName) -> http_v1::HeaderName {
        let bytes = legacy.as_str().as_bytes();
        http_v1::HeaderName::from_bytes(bytes).unwrap()
    }

    fn header_value(legacy: http_legacy::HeaderValue) -> http_v1::HeaderValue {
        let bytes = legacy.as_bytes();
        http_v1::HeaderValue::from_bytes(bytes).unwrap()
    }
}

/// Wraps a legacy v0.4 asynchronous request or response body.
#[pin_project(project = LegacyHyperAdaptorBodyPinned)]
pub struct LegacyHyperAdaptorBody<B> {
    #[pin]
    inner: B,
    /// True if the inner body's data has been polled.
    data_finished: bool,
    /// True if the inner body's stream has been completed.
    stream_finished: bool,
}

// === impl LegacyHyperAdaptorBody ===

impl<B> LegacyHyperAdaptorBody<B>
where
    B: crate::legacy::http_body::Body,
{
    /// Returns a legacy adaptor body.
    pub fn new(body: B) -> Self {
        let empty = body.is_end_stream();

        Self {
            inner: body,
            // These two flags should be true if the inner body is already exhausted.
            data_finished: empty,
            stream_finished: empty,
        }
    }
}

/// A [`LegacyHyperAdaptorBody`] implements the v1.0 [`Body`][body] interface.
///
/// [body]: crate::v1_0::http_body::Body
impl<B> crate::v1_0::http_body::Body for LegacyHyperAdaptorBody<B>
where
    B: crate::legacy::http_body::Body,
{
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<
        Option<
            Result<http_body_1_0::Frame<<Self as crate::v1_0::http_body::Body>::Data>, Self::Error>,
        >,
    > {
        use crate::v1_0::http_body::Frame;

        let LegacyHyperAdaptorBodyPinned {
            inner,
            data_finished,
            stream_finished,
        } = self.project();

        // If the stream is finished, return `None`.
        if *stream_finished {
            return Poll::Ready(None);
        }

        // If all of the body's data has been polled, it is time to poll the trailers.
        if *data_finished {
            let trailers = ready!(inner.poll_trailers(cx))
                .map(|o| o.map(crate::compat::headers).map(Frame::trailers))
                .transpose();
            // Mark this stream as finished once the inner body yields a result.
            *stream_finished = true;
            return Poll::Ready(trailers);
        }

        // If we're here, we should poll the inner body for a chunk of data.
        let data = ready!(inner.poll_data(cx)).map(|r| r.map(Frame::data));
        if data.is_none() {
            // If `None` was yielded, mark the data as finished.
            *data_finished = true;
        } else if matches!(data, Some(Err(_))) {
            // If an error was yielded, the data is finished *and* the stream is finished.
            *data_finished = true;
            *stream_finished = true;
        }

        Poll::Ready(data)
    }

    #[inline]
    fn is_end_stream(&self) -> bool {
        self.stream_finished
    }

    fn size_hint(&self) -> crate::v1_0::http_body::SizeHint {
        let mut size_hint = crate::v1_0::http_body::SizeHint::new();
        let legacy = self.inner.size_hint();

        // Set the lower, upper, and exact values on the emitted size hint.
        size_hint.set_lower(legacy.lower());
        if let Some(upper) = legacy.upper() {
            size_hint.set_upper(upper);
        }
        if let Some(exact) = legacy.exact() {
            size_hint.set_exact(exact);
        }

        size_hint
    }
}

/// A [`LegacyHyperAdaptorBody`] implements the legacy [`Body`][body] interface.
///
/// [body]: crate::legacy::http_body::Body
impl<B> crate::legacy::http_body::Body for LegacyHyperAdaptorBody<B>
where
    B: crate::legacy::http_body::Body,
{
    type Data = B::Data;
    type Error = B::Error;

    #[inline]
    fn poll_data(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<Self::Data, Self::Error>>> {
        self.project().inner.poll_data(cx)
    }

    #[inline]
    fn poll_trailers(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<Option<crate::legacy::http::HeaderMap>, Self::Error>> {
        self.project().inner.poll_trailers(cx)
    }

    #[inline]
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    #[inline]
    fn size_hint(&self) -> crate::legacy::http_body::SizeHint {
        self.inner.size_hint()
    }
}
