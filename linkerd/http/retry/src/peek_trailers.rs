use futures::{
    future::{self, Either},
    FutureExt,
};
use http::HeaderMap;
use http_body::Body;
use linkerd_http_box::BoxBody;
use pin_project::pin_project;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

/// An HTTP body that allows inspecting the body's trailers, if a `TRAILERS`
/// frame was the first frame after the initial headers frame.
///
/// If the first frame of the body stream was *not* a `TRAILERS` frame, this
/// behaves identically to a normal body.
#[pin_project]
pub struct PeekTrailersBody<B: Body = BoxBody>(#[pin] Inner<B>);

#[pin_project(project = Projection)]
enum Inner<B: Body = BoxBody> {
    /// An empty body.
    Empty,
    /// A body that contains zero or one DATA frame.
    ///
    /// This variant MAY have trailers that can be peeked.
    Unary {
        data: Option<Result<B::Data, B::Error>>,
        trailers: Option<Result<HeaderMap, B::Error>>,
    },
    /// A body that (potentially) contains more than one DATA frame.
    ///
    /// This variant indicates that the inner body's trailers could not be observed, with some
    /// frames that were buffered.
    Buffered {
        first: Option<Result<B::Data, B::Error>>,
        second: Option<Result<B::Data, B::Error>>,
        /// The inner [`Body`].
        #[pin]
        inner: B,
    },
    /// A transparent, inert body.
    ///
    /// This variant will not attempt to peek the inner body's trailers.
    Passthru {
        /// The inner [`Body`].
        #[pin]
        inner: B,
    },
}

/// A future that yields a response instrumented with [`PeekTrailersBody<B>`].
pub type WithPeekTrailersBody<B> = Either<ReadyResponse<B>, ReadingResponse<B>>;
/// A future that immediately yields a response.
type ReadyResponse<B> = future::Ready<http::Response<PeekTrailersBody<B>>>;
/// A boxed future that must poll a body before yielding a response.
type ReadingResponse<B> =
    Pin<Box<dyn Future<Output = http::Response<PeekTrailersBody<B>>> + Send + 'static>>;

// === impl WithTrailers ===

impl<B: Body> PeekTrailersBody<B> {
    /// Returns a reference to the body's trailers, if available.
    ///
    /// This function will return `None` if the body's trailers could not be peeked, or if there
    /// were no trailers included.
    pub fn peek_trailers(&self) -> Option<&http::HeaderMap> {
        let Self(inner) = self;
        match inner {
            Inner::Unary {
                trailers: Some(Ok(trailers)),
                ..
            } => Some(trailers),
            Inner::Unary {
                trailers: None | Some(Err(_)),
                ..
            }
            | Inner::Empty
            | Inner::Buffered { .. }
            | Inner::Passthru { .. } => None,
        }
    }

    pub fn map_response(rsp: http::Response<B>) -> WithPeekTrailersBody<B>
    where
        B: Send + Unpin + 'static,
        B::Data: Send + Unpin + 'static,
        B::Error: Send,
    {
        use http::Version;

        // If the response isn't an HTTP version that has trailers, skip trying
        // to read a trailers frame.
        if let Version::HTTP_09 | Version::HTTP_10 | Version::HTTP_11 = rsp.version() {
            return Either::Left(future::ready(
                rsp.map(|inner| Self(Inner::Passthru { inner })),
            ));
        }

        // If the response doesn't have a body stream, also skip trying to read
        // a trailers frame.
        if rsp.is_end_stream() {
            tracing::debug!("Skipping trailers for empty body");
            return Either::Left(future::ready(rsp.map(|_| Self(Inner::Empty))));
        }

        // Otherwise, return a future that tries to read the next frame.
        Either::Right(Box::pin(async move {
            let (parts, body) = rsp.into_parts();
            let body = Self::read_body(body).await;
            http::Response::from_parts(parts, body)
        }))
    }

    async fn read_body(body: B) -> Self
    where
        B: Send + Unpin,
        B::Data: Send + Unpin,
        B::Error: Send,
    {
        // XXX(kate): for now, wrap this in a compatibility adapter that yields `Frame<T>`s.
        // this can be removed when we upgrade to http-body 1.0.
        use crate::compat::ForwardCompatibleBody;
        let mut body = ForwardCompatibleBody::new(body);

        // First, poll the body for its first frame.
        tracing::debug!("Buffering first data frame");
        let first_frame = body
            .frame()
            .map(|f| f.map(|r| r.map(Self::split_frame)))
            .await;

        let body = Self(match first_frame {
            // The body has no frames. It is empty.
            None => Inner::Empty,
            // The body yielded an error. We are done.
            Some(Err(error)) => Inner::Unary {
                data: Some(Err(error)),
                trailers: None,
            },
            // The body yielded a TRAILERS frame. We are done.
            Some(Ok(Some(Either::Right(trailers)))) => Inner::Unary {
                data: None,
                trailers: Some(Ok(trailers)),
            },
            // The body yielded an unknown kind of frame.
            Some(Ok(None)) => Inner::Buffered {
                first: None,
                second: None,
                inner: body.into_inner(),
            },
            // The body yielded a DATA frame. Check for a second frame, without yielding again.
            Some(Ok(Some(Either::Left(first)))) => {
                if let Some(second) = body
                    .frame()
                    .map(|f| f.map(|r| r.map(Self::split_frame)))
                    .now_or_never()
                {
                    // The second frame is available. Let's inspect it and determine what to do.
                    match second {
                        // The body is finished. There is not a TRAILERS frame.
                        None => Inner::Unary {
                            data: Some(Ok(first)),
                            trailers: None,
                        },
                        // We immediately yielded a result, but it was an error. Alas!
                        Some(Err(error)) => Inner::Unary {
                            data: Some(Ok(first)),
                            trailers: Some(Err(error)),
                        },
                        // We immediately yielded another frame, but it was a second DATA frame.
                        // We hold on to each frame, but we cannot wait for the TRAILERS.
                        Some(Ok(Some(Either::Left(second)))) => Inner::Buffered {
                            first: Some(Ok(first)),
                            second: Some(Ok(second)),
                            inner: body.into_inner(),
                        },
                        // The body immediately yielded a second TRAILERS frame. Nice!
                        Some(Ok(Some(Either::Right(trailers)))) => Inner::Unary {
                            data: Some(Ok(first)),
                            trailers: Some(Ok(trailers)),
                        },
                        // The body yielded an unknown kind of frame.
                        Some(Ok(None)) => Inner::Buffered {
                            first: None,
                            second: None,
                            inner: body.into_inner(),
                        },
                    }
                } else {
                    // If we are here, the second frame is not yet available. We cannot be sure
                    // that a second DATA frame is on the way, and we are no longer willing to
                    // await additional frames. There are no trailers to peek.
                    Inner::Buffered {
                        first: None,
                        second: None,
                        inner: body.into_inner(),
                    }
                }
            }
        });

        if body.peek_trailers().is_some() {
            tracing::debug!("Buffered trailers frame");
        }

        body
    }

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

impl<B> Body for PeekTrailersBody<B>
where
    B: Body + Unpin,
    B::Data: Unpin,
    B::Error: Unpin,
{
    type Data = B::Data;
    type Error = B::Error;

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        let this = self.project().0.project();
        match this {
            Projection::Empty => Poll::Ready(None),
            Projection::Passthru { inner } => inner.poll_data(cx),
            Projection::Unary { data, .. } => Poll::Ready(data.take()),
            Projection::Buffered {
                first,
                second,
                inner,
            } => {
                if let data @ Some(_) = first.take().or_else(|| second.take()) {
                    Poll::Ready(data)
                } else {
                    inner.poll_data(cx)
                }
            }
        }
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<http::HeaderMap>, Self::Error>> {
        let this = self.project().0.project();
        match this {
            Projection::Empty => Poll::Ready(Ok(None)),
            Projection::Passthru { inner } => inner.poll_trailers(cx),
            Projection::Unary { trailers, .. } => Poll::Ready(trailers.take().transpose()),
            Projection::Buffered { inner, .. } => inner.poll_trailers(cx),
        }
    }

    #[inline]
    fn is_end_stream(&self) -> bool {
        let Self(inner) = self;
        match inner {
            Inner::Empty => true,
            Inner::Passthru { inner } => inner.is_end_stream(),
            Inner::Unary {
                data: None,
                trailers: None,
            } => true,
            Inner::Unary { .. } => false,
            Inner::Buffered {
                inner,
                first: None,
                second: None,
            } => inner.is_end_stream(),
            Inner::Buffered { .. } => false,
        }
    }

    #[inline]
    fn size_hint(&self) -> http_body::SizeHint {
        use bytes::Buf;
        let Self(inner) = self;
        match inner {
            Inner::Empty => http_body::SizeHint::new(),
            Inner::Passthru { inner } => inner.size_hint(),
            Inner::Unary {
                data: Some(Ok(data)),
                ..
            } => {
                let size = data.remaining() as u64;
                http_body::SizeHint::with_exact(size)
            }
            Inner::Unary {
                data: None | Some(Err(_)),
                ..
            } => http_body::SizeHint::new(),
            Inner::Buffered {
                first,
                second,
                inner,
            } => {
                // Add any frames we've buffered to the inner `Body`'s size hint.
                let mut hint = inner.size_hint();
                let mut add_to_hint = |frame: &Option<Result<B::Data, B::Error>>| {
                    if let Some(Ok(buf)) = frame {
                        let size = buf.remaining() as u64;
                        if let Some(upper) = hint.upper() {
                            hint.set_upper(upper + size);
                        }
                        hint.set_lower(hint.lower() + size);
                    }
                };
                add_to_hint(first);
                add_to_hint(second);
                hint
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PeekTrailersBody;
    use bytes::Bytes;
    use http::{HeaderMap, HeaderValue};
    use http_body::Body;
    use linkerd_error::Error;
    use std::{
        collections::VecDeque,
        ops::Not,
        pin::Pin,
        task::{Context, Poll},
    };

    /// A "mock" body.
    ///
    /// This type contains polling results for [`Body`].
    #[derive(Default)]
    struct MockBody {
        data_polls: VecDeque<Poll<Option<Result<Bytes, Error>>>>,
        trailer_polls: VecDeque<Poll<Result<Option<http::HeaderMap>, Error>>>,
    }

    fn data() -> Option<Result<Bytes, Error>> {
        let bytes = Bytes::from_static(b"hello");
        Some(Ok(bytes))
    }

    fn trailers() -> Result<Option<http::HeaderMap>, Error> {
        let mut trls = HeaderMap::with_capacity(1);
        let value = HeaderValue::from_static("shiny");
        trls.insert("trailer", value);
        Ok(Some(trls))
    }

    #[tokio::test]
    async fn cannot_peek_empty() {
        let (_guard, _handle) = linkerd_tracing::test::trace_init();
        let empty = MockBody::default();
        let peek = PeekTrailersBody::read_body(empty).await;
        assert!(peek.peek_trailers().is_none());
        assert!(peek.is_end_stream());
    }

    #[tokio::test]
    async fn peeks_only_trailers() {
        let (_guard, _handle) = linkerd_tracing::test::trace_init();
        let only_trailers = MockBody::default().then_yield_trailer(Poll::Ready(trailers()));
        let peek = PeekTrailersBody::read_body(only_trailers).await;
        assert!(peek.peek_trailers().is_some());
        assert!(peek.is_end_stream().not());
    }

    #[tokio::test]
    async fn peeks_one_frame_with_immediate_trailers() {
        let (_guard, _handle) = linkerd_tracing::test::trace_init();
        let body = MockBody::default()
            .then_yield_data(Poll::Ready(data()))
            .then_yield_trailer(Poll::Ready(trailers()));
        let peek = PeekTrailersBody::read_body(body).await;
        assert!(peek.peek_trailers().is_some());
        assert!(peek.is_end_stream().not());
    }

    #[tokio::test]
    async fn cannot_peek_one_frame_with_eventual_trailers() {
        let (_guard, _handle) = linkerd_tracing::test::trace_init();
        let body = MockBody::default()
            .then_yield_data(Poll::Ready(data()))
            .then_yield_trailer(Poll::Pending)
            .then_yield_trailer(Poll::Ready(trailers()));
        let peek = PeekTrailersBody::read_body(body).await;
        assert!(peek.peek_trailers().is_none());
        assert!(peek.is_end_stream().not());
    }

    #[tokio::test]
    async fn peeks_one_eventual_frame_with_immediate_trailers() {
        let (_guard, _handle) = linkerd_tracing::test::trace_init();
        let body = MockBody::default()
            .then_yield_data(Poll::Pending)
            .then_yield_data(Poll::Ready(data()))
            .then_yield_trailer(Poll::Ready(trailers()));
        let peek = PeekTrailersBody::read_body(body).await;
        assert!(peek.peek_trailers().is_some());
        assert!(peek.is_end_stream().not());
    }

    #[tokio::test]
    async fn cannot_peek_two_frames_with_immediate_trailers() {
        let (_guard, _handle) = linkerd_tracing::test::trace_init();
        let body = MockBody::default()
            .then_yield_data(Poll::Ready(data()))
            .then_yield_data(Poll::Ready(data()))
            .then_yield_trailer(Poll::Ready(trailers()));
        let peek = PeekTrailersBody::read_body(body).await;
        assert!(peek.peek_trailers().is_none());
        assert!(peek.is_end_stream().not());
    }

    // === impl MockBody ===

    impl MockBody {
        /// Appends a poll outcome for [`Body::poll_data()`].
        fn then_yield_data(mut self, poll: Poll<Option<Result<Bytes, Error>>>) -> Self {
            self.data_polls.push_back(poll);
            self
        }

        /// Appends a poll outcome for [`Body::poll_trailers()`].
        fn then_yield_trailer(
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
                // The called has polled for trailers before exhausting the stream of DATA frames.
                // This indicates `PeekTrailersBody<B>` isn't respecting the contract outlined in
                // <https://docs.rs/http-body/0.4.6/http_body/trait.Body.html#tymethod.poll_trailers>.
                panic!("`poll_trailers()` was called before `poll_data()` returned `Poll::Ready(None)`");
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
}
