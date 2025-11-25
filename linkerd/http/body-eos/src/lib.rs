//! [`Body`] middleware that calls a function when the body ends.

use http::HeaderMap;
use http_body::{Body, Frame};
use linkerd_error::Error;
use std::{
    pin::Pin,
    task::{Context, Poll},
};

#[cfg(test)]
mod tests;

/// A [`Body`] that calls a function when the body ends.
#[pin_project::pin_project(PinnedDrop, project = BodyWithEosFnProj)]
pub struct BodyWithEosFn<B, F>
where
    B: Body,
    F: FnOnce(EosRef<<B as Body>::Error>),
{
    #[pin]
    inner: B,
    callback: Option<F>,
}

/// A reference to the end of a [`Body`] stream.
pub enum EosRef<'a, E = Error> {
    None,
    Trailers(&'a HeaderMap),
    Error(&'a E),
    Cancelled,
}

// === imple BodyWithEosFn ===

impl<B, F> BodyWithEosFn<B, F>
where
    B: Body,
    F: FnOnce(EosRef<<B as Body>::Error>),
{
    /// Returns a new [`BodyWithEosFn<B, F>`].
    pub fn new(body: B, f: F) -> Self {
        let callback = if body.is_end_stream() {
            // If the body is empty, invoke the callback immediately.
            f(EosRef::None);
            None
        } else {
            // Otherwise, hold the callback until the end of the stream is reached.
            Some(f)
        };

        Self {
            inner: body,
            callback,
        }
    }

    /// Returns an [`EosRef<'a, E>`] view of the end-of-stream, if applicable.
    fn eos_ref<'frame, T, E>(
        frame: &'frame Option<Result<Frame<T>, E>>,
        body: &B,
    ) -> Option<EosRef<'frame, E>> {
        let frame = match frame {
            Some(Ok(f)) => f,
            // Errors indicate the end of the stream.
            Some(Err(error)) => return Some(EosRef::Error(error)),
            // If nothing was yielded, the end of the stream has been reached.
            None => return Some(EosRef::None),
        };

        if let Some(trls) = frame.trailers_ref() {
            // A trailers frame indicates the end of the stream.
            Some(EosRef::Trailers(trls))
        } else {
            // `is_end_stream()` hints that the end of the stream has been reached.
            body.is_end_stream().then_some(EosRef::None)
        }
    }
}

impl<B, F> http_body::Body for BodyWithEosFn<B, F>
where
    B: Body,
    F: FnOnce(EosRef<<B as Body>::Error>),
{
    type Data = <B as Body>::Data;
    type Error = <B as Body>::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let BodyWithEosFnProj {
            mut inner,
            callback,
        } = self.project();

        // Poll the inner body for the next frame.
        let poll = inner.as_mut().poll_frame(cx);
        let frame = futures::ready!(poll);

        // Invoke the callback if we have reached the end of the stream.
        if let Some(eos) = Self::eos_ref(&frame, &inner) {
            if let Some(callback) = callback.take() {
                callback(eos);
            }
        }

        Poll::Ready(frame)
    }

    fn is_end_stream(&self) -> bool {
        let Self { inner: _, callback } = self;

        callback.is_none()
    }
}

#[pin_project::pinned_drop]
impl<B, F> PinnedDrop for BodyWithEosFn<B, F>
where
    B: Body,
    F: FnOnce(EosRef<<B as Body>::Error>),
{
    fn drop(self: Pin<&mut Self>) {
        let BodyWithEosFnProj { inner: _, callback } = self.project();
        if let Some(callback) = callback.take() {
            // Invoke the callback if the body was dropped before finishing.
            callback(EosRef::Cancelled)
        };
    }
}

// === impl EosRef ===

/// A reference to the end of a stream can be cheaply cloned.
///
/// Each of the variants are empty tags, or immutable references.
impl<'a, E> Clone for EosRef<'a, E> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'a, E> Copy for EosRef<'a, E> {}
