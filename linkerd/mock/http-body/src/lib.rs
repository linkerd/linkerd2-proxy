//! Mock [`http_body::Body`] facilities for use in tests.
//!
//! See [`MockBody`] for more information.

use bytes::Bytes;
use http_body::{Body, Frame};
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
    trailer_polls: VecDeque<Poll<Option<Result<http::HeaderMap, Error>>>>,
}

// === impl MockBody ===

impl MockBody {
    /// Appends a poll outcome for [`Body::poll_frame()`].
    pub fn then_yield_data(mut self, poll: Poll<Option<Result<Bytes, Error>>>) -> Self {
        self.data_polls.push_back(poll);
        self
    }

    /// Appends a [`Poll`] outcome for [`Body::poll_frame()`].
    ///
    /// These this will be yielded after data has been polled.
    pub fn then_yield_trailer(
        mut self,
        poll: Poll<Option<Result<http::HeaderMap, Error>>>,
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

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let Self {
            data_polls,
            trailer_polls,
        } = self.get_mut();

        let poll = {
            let mut next_data = || data_polls.pop_front().map(|p| p.map_ok(Frame::data));
            let next_trailer = || trailer_polls.pop_front().map(|p| p.map_ok(Frame::trailers));
            next_data()
                .or_else(next_trailer)
                .unwrap_or(Poll::Ready(None))
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
