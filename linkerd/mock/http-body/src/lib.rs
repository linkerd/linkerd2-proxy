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
pub struct MockBody {
    data_polls: VecDeque<Poll<Option<Result<Bytes, Error>>>>,
    trailer_polls: VecDeque<Poll<Option<Result<http::HeaderMap, Error>>>>,
    /// If true, [`Body::is_end_stream()`] will report when the body is exhausted.
    report_eos: bool,
}

// === impl MockBody ===

impl Default for MockBody {
    fn default() -> Self {
        Self {
            data_polls: Default::default(),
            trailer_polls: Default::default(),
            report_eos: true,
        }
    }
}

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

    /// Disables end-of-stream reporting via [`Body::is_end_stream()`].
    ///
    /// Bodies always return `false` by default; this can help write tests exercising this.
    pub fn without_eos(mut self) -> Self {
        self.report_eos = false;
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
            report_eos: _,
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
        let Self {
            data_polls,
            trailer_polls,
            report_eos,
        } = self;

        *report_eos && data_polls.is_empty() && trailer_polls.is_empty()
    }
}
