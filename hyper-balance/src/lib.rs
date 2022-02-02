#![deny(
    warnings,
    rust_2018_idioms,
    clippy::disallowed_method,
    clippy::disallowed_type
)]
#![forbid(unsafe_code)]

use hyper::body::HttpBody;
use pin_project::pin_project;
use std::pin::Pin;
use std::task::{Context, Poll};
use tower::load::TrackCompletion;

/// Instruments HTTP responses to drop handles when their first body message is received.
#[derive(Clone, Debug, Default)]
pub struct PendingUntilFirstData(());

/// Instruments HTTP responses to drop handles when their streams completes.
#[derive(Clone, Debug, Default)]
pub struct PendingUntilEos(());

/// An instrumented HTTP body that drops its handle when the first data is
/// received.
#[pin_project]
#[derive(Debug)]
pub struct PendingUntilFirstDataBody<T, B> {
    handle: Option<T>,
    #[pin]
    body: B,
}

/// An instrumented HTTP body that drops its handle upon completion.
#[pin_project]
#[derive(Debug)]
pub struct PendingUntilEosBody<T, B> {
    handle: Option<T>,
    #[pin]
    body: B,
}

// ==== PendingUntilFirstData ====

impl<T, B> TrackCompletion<T, http::Response<B>> for PendingUntilFirstData
where
    B: HttpBody,
{
    type Output = http::Response<PendingUntilFirstDataBody<T, B>>;

    fn track_completion(&self, handle: T, rsp: http::Response<B>) -> Self::Output {
        rsp.map(move |body| {
            let handle = if body.is_end_stream() {
                drop(handle);
                None
            } else {
                Some(handle)
            };
            PendingUntilFirstDataBody { handle, body }
        })
    }
}

// ==== PendingUntilEos ====

impl<T, B> TrackCompletion<T, http::Response<B>> for PendingUntilEos
where
    B: HttpBody,
{
    type Output = http::Response<PendingUntilEosBody<T, B>>;

    fn track_completion(&self, handle: T, rsp: http::Response<B>) -> Self::Output {
        rsp.map(move |body| {
            let handle = if body.is_end_stream() {
                drop(handle);
                None
            } else {
                Some(handle)
            };
            PendingUntilEosBody { handle, body }
        })
    }
}

// ==== PendingUntilFirstDataBody ====

impl<T, B> Default for PendingUntilFirstDataBody<T, B>
where
    B: HttpBody + Default,
{
    fn default() -> Self {
        Self {
            body: B::default(),
            handle: None,
        }
    }
}

impl<T, B> HttpBody for PendingUntilFirstDataBody<T, B>
where
    B: HttpBody,
    T: Send + 'static,
{
    type Data = B::Data;
    type Error = B::Error;

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        let this = self.project();
        let ret = futures::ready!(this.body.poll_data(cx));

        // Once a data frame is received, the handle is dropped. On subsequent calls, this
        // is a noop.
        drop(this.handle.take());

        Poll::Ready(ret)
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<http::HeaderMap>, Self::Error>> {
        let this = self.project();
        // If this is being called, the handle definitely should have been dropped
        // already.
        drop(this.handle.take());

        this.body.poll_trailers(cx)
    }

    #[inline]
    fn size_hint(&self) -> hyper::body::SizeHint {
        self.body.size_hint()
    }
}

// ==== PendingUntilEosBody ====

impl<T, B> Default for PendingUntilEosBody<T, B>
where
    B: HttpBody + Default,
{
    fn default() -> Self {
        Self {
            body: B::default(),
            handle: None,
        }
    }
}

impl<T: Send + 'static, B: HttpBody> HttpBody for PendingUntilEosBody<T, B> {
    type Data = B::Data;
    type Error = B::Error;

    #[inline]
    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        let mut this = self.project();
        let body = &mut this.body;
        tokio::pin!(body);
        let ret = futures::ready!(body.poll_data(cx));

        // If this was the last frame, then drop the handle immediately.
        if this.body.is_end_stream() {
            drop(this.handle.take());
        }

        Poll::Ready(ret)
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<http::HeaderMap>, Self::Error>> {
        let this = self.project();
        let ret = futures::ready!(this.body.poll_trailers(cx));

        // Once trailers are received, the handle is dropped immediately (in case the body
        // is retained longer for some reason).
        drop(this.handle.take());

        Poll::Ready(ret)
    }

    #[inline]
    fn size_hint(&self) -> hyper::body::SizeHint {
        self.body.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::{PendingUntilEos, PendingUntilFirstData};
    use futures::future::poll_fn;
    use hyper::body::HttpBody;
    use std::collections::VecDeque;
    use std::io::Cursor;
    use std::pin::Pin;
    use std::sync::{Arc, Weak};
    use std::task::{Context, Poll};
    use tokio_test::{assert_ready, task};
    use tower::load::TrackCompletion;

    #[test]
    fn first_data() {
        let body = {
            let mut parts = VecDeque::new();
            parts.push_back("one");
            TestBody(parts, None)
        };

        let (h, wk) = Handle::new();
        let (_, mut body) = PendingUntilFirstData::default()
            .track_completion(h, http::Response::new(body))
            .into_parts();

        assert!(wk.upgrade().is_some());

        assert_ready!(task::spawn(poll_fn(|cx| {
            let body = &mut body;
            tokio::pin!(body);
            body.poll_data(cx)
        }))
        .poll())
        .expect("data some")
        .expect("data ok");
        assert!(wk.upgrade().is_none());
    }

    #[test]
    fn first_data_empty() {
        let body = TestBody::default();

        let (h, wk) = Handle::new();
        let (_, _body) = PendingUntilFirstData::default()
            .track_completion(h, http::Response::new(body))
            .into_parts();
        assert!(wk.upgrade().is_none());
    }

    #[test]
    fn first_data_drop() {
        let body = {
            let mut parts = VecDeque::new();
            parts.push_back("one");
            TestBody(parts, None)
        };

        let (h, wk) = Handle::new();
        let (_, body) = PendingUntilFirstData::default()
            .track_completion(h, http::Response::new(body))
            .into_parts();

        assert!(wk.upgrade().is_some());

        drop(body);
        assert!(wk.upgrade().is_none());
    }

    #[test]
    fn first_data_error() {
        let body = {
            let mut parts = VecDeque::new();
            parts.push_back("one");
            parts.push_back("two");
            ErrBody(Some("body error"))
        };

        let (h, wk) = Handle::new();
        let (_, mut body) = PendingUntilFirstData::default()
            .track_completion(h, http::Response::new(body))
            .into_parts();

        assert!(wk.upgrade().is_some());

        let res = assert_ready!(task::spawn(poll_fn(|cx| {
            let body = &mut body;
            tokio::pin!(body);
            body.poll_data(cx)
        }))
        .poll());
        assert!(res.expect("data is some").is_err());
        assert!(wk.upgrade().is_none());
    }

    #[test]
    fn eos() {
        let body = {
            let mut parts = VecDeque::new();
            parts.push_back("one");
            parts.push_back("two");
            TestBody(parts, None)
        };

        let (h, wk) = Handle::new();
        let (_, mut body) = PendingUntilEos::default()
            .track_completion(h, http::Response::new(body))
            .into_parts();

        assert!(wk.upgrade().is_some());

        assert_ready!(task::spawn(poll_fn(|cx| {
            let body = &mut body;
            tokio::pin!(body);
            body.poll_data(cx)
        }))
        .poll())
        .expect("data some")
        .expect("data ok");
        assert!(wk.upgrade().is_some());

        assert_ready!(task::spawn(poll_fn(|cx| {
            let body = &mut body;
            tokio::pin!(body);
            body.poll_data(cx)
        }))
        .poll())
        .expect("data some")
        .expect("data ok");
        assert!(wk.upgrade().is_none());
    }

    #[test]
    fn eos_empty() {
        let body = TestBody::default();

        let (h, wk) = Handle::new();
        let (_, _body) = PendingUntilEos::default()
            .track_completion(h, http::Response::new(body))
            .into_parts();
        assert!(wk.upgrade().is_none());
    }

    #[test]
    fn eos_trailers() {
        let body = {
            let mut parts = VecDeque::new();
            parts.push_back("one");
            parts.push_back("two");
            TestBody(parts, Some(http::HeaderMap::default()))
        };

        let (h, wk) = Handle::new();
        let (_, mut body) = PendingUntilEos::default()
            .track_completion(h, http::Response::new(body))
            .into_parts();
        assert!(wk.upgrade().is_some());

        assert_ready!(task::spawn(poll_fn(|cx| {
            let body = &mut body;
            tokio::pin!(body);
            body.poll_data(cx)
        }))
        .poll())
        .expect("data")
        .expect("data ok");
        assert!(wk.upgrade().is_some());

        assert_ready!(task::spawn(poll_fn(|cx| {
            let body = &mut body;
            tokio::pin!(body);
            body.poll_data(cx)
        }))
        .poll())
        .expect("data")
        .expect("data ok");
        assert!(wk.upgrade().is_some());

        let poll = assert_ready!(task::spawn(poll_fn(|cx| {
            let body = &mut body;
            tokio::pin!(body);
            body.poll_data(cx)
        }))
        .poll());
        assert!(poll.is_none());
        assert!(wk.upgrade().is_some());

        assert_ready!(task::spawn(poll_fn(|cx| {
            let body = &mut body;
            tokio::pin!(body);
            body.poll_trailers(cx)
        }))
        .poll())
        .expect("trailers ok")
        .expect("trailers");
        assert!(wk.upgrade().is_none());
    }

    #[test]
    fn eos_error() {
        let body = {
            let mut parts = VecDeque::new();
            parts.push_back("one");
            parts.push_back("two");
            ErrBody(Some("eos error"))
        };

        let (h, wk) = Handle::new();
        let (_, mut body) = PendingUntilEos::default()
            .track_completion(h, http::Response::new(body))
            .into_parts();

        assert!(wk.upgrade().is_some());

        let poll = assert_ready!(task::spawn(poll_fn(|cx| {
            let body = &mut body;
            tokio::pin!(body);
            body.poll_data(cx)
        }))
        .poll());
        assert!(poll.expect("some").is_err());
        assert!(wk.upgrade().is_none());
    }

    struct Handle(Arc<()>);
    impl Handle {
        fn new() -> (Self, Weak<()>) {
            let strong = Arc::new(());
            let weak = Arc::downgrade(&strong);
            (Handle(strong), weak)
        }
    }

    #[derive(Default)]
    struct TestBody(VecDeque<&'static str>, Option<http::HeaderMap>);
    impl HttpBody for TestBody {
        type Data = Cursor<&'static str>;
        type Error = &'static str;

        fn is_end_stream(&self) -> bool {
            self.0.is_empty() & self.1.is_none()
        }

        fn poll_data(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
            Poll::Ready(self.as_mut().0.pop_front().map(Cursor::new).map(Ok))
        }

        fn poll_trailers(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Result<Option<http::HeaderMap>, Self::Error>> {
            let mut this = self.as_mut();
            assert!(this.0.is_empty());
            Poll::Ready(Ok(this.1.take()))
        }
    }

    #[derive(Default)]
    struct ErrBody(Option<&'static str>);
    impl HttpBody for ErrBody {
        type Data = Cursor<&'static str>;
        type Error = &'static str;

        fn is_end_stream(&self) -> bool {
            self.0.is_none()
        }

        fn poll_data(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
            Poll::Ready(Some(Err(self.as_mut().0.take().expect("err"))))
        }

        fn poll_trailers(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Result<Option<http::HeaderMap>, Self::Error>> {
            Poll::Ready(Err(self.as_mut().0.take().expect("err")))
        }
    }
}
