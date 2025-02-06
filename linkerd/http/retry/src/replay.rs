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
    rest: B,
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
                rest: body,
                is_completed: false,
                max_bytes: max_bytes + 1,
            }),
            // The initial `ReplayBody` has nothing to replay
            replay_body: false,
            replay_trailers: false,
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
        let data = {
            tracing::trace!("Polling initial body");
            match futures::ready!(Pin::new(&mut state.rest).poll_data(cx)) {
                Some(Ok(data)) => data,
                Some(Err(e)) => return Poll::Ready(Some(Err(e.into()))),
                None => {
                    tracing::trace!("Initial body completed");
                    state.is_completed = true;
                    return Poll::Ready(None);
                }
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
        cx: &mut Context<'_>,
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

        // If the inner body has previously ended, don't poll it again.
        if !state.rest.is_end_stream() {
            return Pin::new(&mut state.rest)
                .poll_trailers(cx)
                .map_ok(|tlrs| {
                    // Record a copy of the inner body's trailers in the shared state.
                    if state.trailers.is_none() {
                        state.trailers.clone_from(&tlrs);
                    }
                    tlrs
                })
                .map_err(Into::into);
        }

        Poll::Ready(Ok(None))
    }

    fn is_end_stream(&self) -> bool {
        // If the initial body was empty as soon as it was wrapped, then we are finished.
        if self.shared.was_empty {
            return true;
        }

        let Some(state) = self.state.as_ref() else {
            // This body is not currently the "active" replay being polled.
            return false;
        };

        // if this body has data or trailers remaining to play back, it
        // is not EOS
        !self.replay_body && !self.replay_trailers
            // if we have replayed everything, the initial body may
            // still have data remaining, so ask it
            && state.rest.is_end_stream()
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

// === impl BodyState ===

impl<B> BodyState<B> {
    #[inline]
    fn is_capped(&self) -> bool {
        self.max_bytes == 0
    }
}
