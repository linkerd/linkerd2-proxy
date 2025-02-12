//! Mock [`http_body::Body`] facilities for use in tests.
//!
//! See [`MockBody`] for more information.

use bytes::Bytes;
use http_body::Body;
use linkerd_error::Error;
use std::{
    collections::VecDeque,
    pin::Pin,
    task::{Context, Poll},
};

/// A "mock" body.
///
/// This type contains polling results for [`Body`].
#[derive(Default)]
pub struct MockBody {
    data_polls: VecDeque<Poll<Option<Result<Bytes, Error>>>>,
    trailer_polls: VecDeque<Poll<Result<Option<http::HeaderMap>, Error>>>,
}

// === impl MockBody ===

impl MockBody {
    /// Appends a poll outcome for [`Body::poll_data()`].
    pub fn then_yield_data(mut self, poll: Poll<Option<Result<Bytes, Error>>>) -> Self {
        self.data_polls.push_back(poll);
        self
    }

    /// Appends a poll outcome for [`Body::poll_trailers()`].
    pub fn then_yield_trailer(
        mut self,
        poll: Poll<Result<Option<http::HeaderMap>, Error>>,
    ) -> Self {
        self.trailer_polls.push_back(poll);
        self
    }

    /// Schedules a task to be awoken.
    fn schedule(cx: &Context<'_>) {
        let waker = cx.waker().clone();
        tokio::spawn(async move {
            waker.wake();
        });
    }
}

impl Body for MockBody {
    type Data = Bytes;
    type Error = Error;

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        let poll = self
            .get_mut()
            .data_polls
            .pop_front()
            .unwrap_or(Poll::Ready(None));
        // If we return `Poll::Pending`, we must schedule the task to be awoken.
        if poll.is_pending() {
            Self::schedule(cx);
        }
        poll
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<http::HeaderMap>, Self::Error>> {
        let Self {
            data_polls,
            trailer_polls,
        } = self.get_mut();

        let poll = if data_polls.is_empty() {
            trailer_polls.pop_front().unwrap_or(Poll::Ready(Ok(None)))
        } else {
            // The caller has polled for trailers before exhausting the stream of DATA frames.
            // This indicates `PeekTrailersBody<B>` isn't respecting the contract outlined in
            // <https://docs.rs/http-body/0.4.6/http_body/trait.Body.html#tymethod.poll_trailers>.
            panic!(
                "`poll_trailers()` was called before `poll_data()` returned `Poll::Ready(None)`"
            );
        };

        // If we return `Poll::Pending`, we must schedule the task to be awoken.
        if poll.is_pending() {
            Self::schedule(cx);
        }

        poll
    }

    fn is_end_stream(&self) -> bool {
        self.data_polls.is_empty() && self.trailer_polls.is_empty()
    }
}
