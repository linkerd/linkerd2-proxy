use bytes::Buf;
use http::HeaderMap;
use http_body::{Body, SizeHint};
use linkerd_error::Error;
use linkerd_http_box::BoxBody;
use parking_lot::Mutex;
use std::{pin::Pin, sync::Arc, task::Context, task::Poll};
use thiserror::Error;

pub use self::buffer::{Data, Replay};

mod buffer;

/// Unit tests for [`ReplayBody<B>`].
#[cfg(test)]
mod tests;

/// Wraps an HTTP body type and lazily buffers data as it is read from the inner
/// body.
///
/// When this body is dropped, if a clone exists, any buffered data is shared
/// with its cloned. The first clone to be polled will take ownership over the
/// data until it is dropped. When *that* clone is dropped, the buffered data
/// --- including any new data read from the body by the clone, if the body has
/// not yet completed --- will be shared with any remaining clones.
///
/// The buffered data can then be used to retry the request if the original
/// request fails.
#[derive(Debug)]
pub struct ReplayBody<B = BoxBody> {
    /// Buffered state owned by this body if it is actively being polled. If
    /// this body has been polled and no other body owned the state, this will
    /// be `Some`.
    state: Option<BodyState<B>>,

    /// Copy of the state shared across all clones. When the active clone is
    /// dropped, it moves its state back into the shared state to be taken by the
    /// next clone to be polled.
    shared: Arc<SharedState<B>>,

    /// Should this clone replay the buffered body from the shared state before
    /// polling the initial body?
    replay_body: bool,

    /// Should this clone replay trailers from the shared state?
    replay_trailers: bool,
}

#[derive(Debug, Error)]
#[error("replay body discarded after reaching maximum buffered bytes limit")]
pub struct Capped;

#[derive(Debug)]
struct SharedState<B> {
    body: Mutex<Option<BodyState<B>>>,
    /// Did the initial body return `true` from `is_end_stream` before it was
    /// ever polled? If so, always return `true`; the body is completely empty.
    ///
    /// We store this separately so that clones of a totally empty body can
    /// always return `true` from `is_end_stream` even when they don't own the
    /// shared state.
    was_empty: bool,

    orig_size_hint: SizeHint,
}

#[derive(Debug)]
struct BodyState<B> {
    replay: Replay,
    trailers: Option<HeaderMap>,
    rest: crate::compat::ForwardCompatibleBody<B>,
    is_completed: bool,

    /// Maximum number of bytes to buffer.
    max_bytes: usize,
}

// === impl ReplayBody ===

impl<B: Body> ReplayBody<B> {
    /// Wraps an initial `Body` in a `ReplayBody`.
    ///
    /// In order to prevent unbounded buffering, this takes a maximum number of bytes to buffer as a
    /// second parameter. If more than than that number of bytes would be buffered, the buffered
    /// data is discarded and any subsequent clones of this body will fail. However, the *currently
    /// active* clone of the body is allowed to continue without erroring. It will simply stop
    /// buffering any additional data for retries.
    ///
    /// If the body has a size hint with a lower bound greater than `max_bytes`, the original body
    /// is returned in the error variant.
    pub fn try_new(body: B, max_bytes: usize) -> Result<Self, B> {
        let orig_size_hint = body.size_hint();
        tracing::trace!(body.size_hint = %orig_size_hint.lower(), %max_bytes);
        if orig_size_hint.lower() > max_bytes as u64 {
            return Err(body);
        }

        Ok(Self {
            shared: Arc::new(SharedState {
                body: Mutex::new(None),
                orig_size_hint,
                was_empty: body.is_end_stream(),
            }),
            state: Some(BodyState {
                replay: Default::default(),
                trailers: None,
                rest: crate::compat::ForwardCompatibleBody::new(body),
                is_completed: false,
                max_bytes: max_bytes + 1,
            }),
            // The initial `ReplayBody` has no data to replay.
            replay_body: false,
            // NOTE(kate): When polling the inner body in terms of frames, we will not yield
            // `Ready(None)` from `Body::poll_data()` until we have reached the end of the
            // underlying stream. Once we have migrated to `http-body` v1, this field will be
            // initialized `false` thanks to the use of `Body::poll_frame()`, but for now we must
            // initialize this to true; `poll_trailers()` will be called after the trailers have
            // been observed previously, even for the initial body.
            replay_trailers: true,
        })
    }

    /// Mutably borrows the body state if this clone currently owns it,
    /// or else tries to acquire it from the shared state.
    ///
    /// # Panics
    ///
    /// This panics if another clone has currently acquired the state, based on
    /// the assumption that a retry body will not be polled until the previous
    /// request has been dropped.
    fn acquire_state<'a>(
        state: &'a mut Option<BodyState<B>>,
        shared: &Mutex<Option<BodyState<B>>>,
    ) -> &'a mut BodyState<B> {
        state.get_or_insert_with(|| shared.lock().take().expect("missing body state"))
    }

    /// Returns `Some(true)` if the body previously exceeded the configured maximum
    /// length limit.
    ///
    /// If this is true, the body is now empty, and the request should *not* be
    /// retried with this body.
    ///
    /// If this is `None`, another clone has currently acquired the state, and is
    /// still being polled.
    pub fn is_capped(&self) -> Option<bool> {
        self.state
            .as_ref()
            .map(BodyState::is_capped)
            .or_else(|| self.shared.body.lock().as_ref().map(BodyState::is_capped))
    }
}

impl<B> Body for ReplayBody<B>
where
    B: Body + Unpin,
    B::Error: Into<Error>,
{
    type Data = Data;
    type Error = Error;

    /// Polls for the next chunk of data in this stream.
    ///
    /// # Panics
    ///
    /// This panics if another clone has currently acquired the state. A [`ReplayBody<B>`] MUST
    /// NOT be polled until the previous body has been dropped.
    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        let this = self.get_mut();
        let state = Self::acquire_state(&mut this.state, &this.shared.body);
        // Move these out to avoid mutable borrow issues in the `map` closure
        // when polling the inner body.
        tracing::trace!(
            replay_body = this.replay_body,
            buf.has_remaining = state.replay.has_remaining(),
            body.is_completed = state.is_completed,
            body.max_bytes_remaining = state.max_bytes,
            "ReplayBody::poll_data"
        );

        // If we haven't replayed the buffer yet, and its not empty, return the
        // buffered data first.
        if this.replay_body {
            if state.replay.has_remaining() {
                tracing::trace!("Replaying body");
                // Don't return the buffered data again on the next poll.
                this.replay_body = false;
                return Poll::Ready(Some(Ok(Data::Replay(state.replay.clone()))));
            }

            if state.is_capped() {
                tracing::trace!("Cannot replay buffered body, maximum buffer length reached");
                return Poll::Ready(Some(Err(Capped.into())));
            }
        }

        // If the inner body has previously ended, don't poll it again.
        //
        // NOTE(eliza): we would expect the inner body to just happily return
        // `None` multiple times here, but `hyper::Body::channel` (which we use
        // in the tests) will panic if it is polled after returning `None`, so
        // we have to special-case this. :/
        if state.is_completed {
            return Poll::Ready(None);
        }

        // Poll the inner body for more data. If the body has ended, remember
        // that so that future clones will not try polling it again (as
        // described above).
        let data: B::Data = {
            use futures::{future::Either, ready};
            // Poll the inner body for the next frame.
            tracing::trace!("Polling initial body");
            let poll = Pin::new(&mut state.rest).poll_frame(cx).map_err(Into::into);
            let frame = match ready!(poll) {
                // The body yielded a new frame.
                Some(Ok(frame)) => frame,
                // The body yielded an error.
                Some(Err(error)) => return Poll::Ready(Some(Err(error))),
                // The body has reached the end of the stream.
                None => {
                    tracing::trace!("Initial body completed");
                    state.is_completed = true;
                    return Poll::Ready(None);
                }
            };
            // Now, inspect the frame: was it a chunk of data, or a trailers frame?
            match Self::split_frame(frame) {
                Some(Either::Left(data)) => data,
                Some(Either::Right(trailers)) => {
                    tracing::trace!("Initial body completed");
                    state.trailers = Some(trailers);
                    state.is_completed = true;
                    return Poll::Ready(None);
                }
                None => return Poll::Ready(None),
            }
        };

        // If we have buffered the maximum number of bytes, allow *this* body to
        // continue, but don't buffer any more.
        let chunk = state.record_bytes(data);

        Poll::Ready(Some(Ok(chunk)))
    }

    /// Polls for an optional **single** [`HeaderMap`] of trailers.
    ///
    /// This function should only be called once `poll_data` returns `None`.
    ///
    /// # Panics
    ///
    /// This panics if another clone has currently acquired the state. A [`ReplayBody<B>`] MUST
    /// NOT be polled until the previous body has been dropped.
    fn poll_trailers(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Result<Option<HeaderMap>, Self::Error>> {
        let this = self.get_mut();
        let state = Self::acquire_state(&mut this.state, &this.shared.body);
        tracing::trace!(
            replay_trailers = this.replay_trailers,
            "Replay::poll_trailers"
        );

        if this.replay_trailers {
            this.replay_trailers = false;
            if let Some(ref trailers) = state.trailers {
                tracing::trace!("Replaying trailers");
                return Poll::Ready(Ok(Some(trailers.clone())));
            }
        }

        Poll::Ready(Ok(None))
    }

    #[tracing::instrument(
        skip_all,
        level = "trace",
        fields(
            state.is_some = %self.state.is_some(),
            replay_trailers = %self.replay_trailers,
            replay_body = %self.replay_body,
            is_completed = ?self.state.as_ref().map(|s| s.is_completed),
        )
    )]
    fn is_end_stream(&self) -> bool {
        // If the initial body was empty as soon as it was wrapped, then we are finished.
        if self.shared.was_empty {
            tracing::trace!("initial body was empty, stream has ended");
            return true;
        }

        let Some(state) = self.state.as_ref() else {
            // This body is not currently the "active" replay being polled.
            tracing::trace!("inactive replay body is not complete");
            return false;
        };

        // if this body has data or trailers remaining to play back, it
        // is not EOS
        let eos = !self.replay_body && !self.replay_trailers
            // if we have replayed everything, the initial body may
            // still have data remaining, so ask it
            && state.rest.is_end_stream();
        tracing::trace!(%eos, "checked replay body end-of-stream");
        eos
    }

    #[inline]
    fn size_hint(&self) -> SizeHint {
        // If this clone isn't holding the body, return the original size hint.
        let state = match self.state.as_ref() {
            Some(state) => state,
            None => return self.shared.orig_size_hint.clone(),
        };

        // Otherwise, if we're holding the state but have dropped the inner
        // body, the entire body is buffered so we know the exact size hint.
        let buffered = state.replay.remaining() as u64;
        let rest_hint = state.rest.size_hint();

        // Otherwise, add the inner body's size hint to the amount of buffered
        // data. An upper limit is only set if the inner body has an upper
        // limit.
        let mut hint = SizeHint::default();
        hint.set_lower(buffered + rest_hint.lower());
        if let Some(rest_upper) = rest_hint.upper() {
            hint.set_upper(buffered + rest_upper);
        }
        hint
    }
}

impl<B> Clone for ReplayBody<B> {
    fn clone(&self) -> Self {
        Self {
            state: None,
            shared: self.shared.clone(),
            // The clone should try to replay from the shared state before
            // reading any additional data from the initial body.
            replay_body: true,
            replay_trailers: true,
        }
    }
}

impl<B> Drop for ReplayBody<B> {
    fn drop(&mut self) {
        // If this clone owned the shared state, put it back.
        if let Some(state) = self.state.take() {
            *self.shared.body.lock() = Some(state);
        }
    }
}

impl<B: Body> ReplayBody<B> {
    /// Splits a `Frame<T>` into a chunk of data or a header map.
    ///
    /// Frames do not expose their inner enums, and instead expose `into_data()` and
    /// `into_trailers()` methods. This function breaks the frame into either `Some(Left(data))`
    /// if it is given a DATA frame, and `Some(Right(trailers))` if it is given a TRAILERS frame.
    ///
    /// This returns `None` if an unknown frame is provided, that is neither.
    ///
    /// This is an internal helper to facilitate pattern matching in `read_body(..)`, above.
    fn split_frame(
        frame: crate::compat::Frame<B::Data>,
    ) -> Option<futures::future::Either<B::Data, HeaderMap>> {
        use {crate::compat::Frame, futures::future::Either};
        match frame.into_data().map_err(Frame::into_trailers) {
            Ok(data) => Some(Either::Left(data)),
            Err(Ok(trailers)) => Some(Either::Right(trailers)),
            Err(Err(_unknown)) => {
                // It's possible that some sort of unknown frame could be encountered.
                tracing::warn!("an unknown body frame has been buffered");
                None
            }
        }
    }
}

// === impl BodyState ===

impl<B> BodyState<B> {
    #[inline]
    fn is_capped(&self) -> bool {
        self.max_bytes == 0
    }
}
