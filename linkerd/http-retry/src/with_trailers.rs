use futures::{
    future::{self, Either},
    FutureExt,
};
use http_body::Body;
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
pub struct WithTrailers<B: Body> {
    inner: B,

    /// The first DATA frame received from the inner body, or an error that
    /// occurred while polling for data.
    ///
    /// If this is `None`, then the body has completed without any DATA frames.
    first_data: Option<Result<B::Data, B::Error>>,

    /// The inner body's trailers, if it was terminated by a `TRAILERS` frame
    /// after 0-1 DATA frames, or an error if polling for trailers failed.
    ///
    /// Yes, this is a bit of a complex type, so let's break it down:
    /// - the outer `Option` indicates whether any trailers were received by
    ///   `WithTrailers`; if it's `None`, then we don't *know* if the response
    ///   had trailers, as it is not yet complete.
    /// - the inner `Result` and `Option` are the `Result` and `Option` returned
    ///   by `HttpBody::trailers` on the inner body. If this is `Ok(None)`, then
    ///   the body has terminated without trailers --- it is *known* to not have
    ///   trailers.
    trailers: Option<Result<Option<http::HeaderMap>, B::Error>>,
}

pub type WithTrailersFuture<B> = Either<
    futures::future::Ready<http::Response<WithTrailers<B>>>,
    Pin<Box<dyn Future<Output = http::Response<WithTrailers<B>>> + Send + 'static>>,
>;

// === impl WithTrailers ===

impl<B: Body> WithTrailers<B> {
    pub fn trailers(&self) -> Option<&http::HeaderMap> {
        self.trailers
            .as_ref()
            .and_then(|trls| trls.as_ref().ok()?.as_ref())
    }

    pub fn map_response(rsp: http::Response<B>) -> WithTrailersFuture<B>
    where
        B: Send + Unpin + 'static,
        B::Data: Send + Unpin + 'static,
        B::Error: Send,
    {
        use http::Version;

        // If the response isn't an HTTP version that has trailers, skip trying
        // to read a trailers frame.
        if let Version::HTTP_09 | Version::HTTP_10 | Version::HTTP_11 = rsp.version() {
            return Either::Left(future::ready(Self::no_trailers(rsp)));
        }

        // If the response doesn't have a body stream, also skip trying to read
        // a trailers frame.
        if rsp.is_end_stream() {
            return Either::Left(future::ready(Self::no_trailers(rsp)));
        }

        // Otherwise, return a future that tries to read the next frame.
        Either::Right(Box::pin(Self::read_response(rsp)))
    }

    async fn read_response(rsp: http::Response<B>) -> http::Response<Self>
    where
        B: Send + Unpin,
        B::Data: Send + Unpin,
        B::Error: Send,
    {
        let (parts, body) = rsp.into_parts();
        let mut body = Self {
            inner: body,
            first_data: None,
            trailers: None,
        };

        if let Some(data) = body.inner.data().await {
            // The body has data; stop waiting for trailers.
            body.first_data = Some(data);

            // Peek to see if there's immediately a trailers frame, and grab
            // it if so. Otherwise, bail.
            // XXX(eliza): the documentation for the `http::Body` trait says
            // that `poll_trailers` should only be called after `poll_data`
            // returns `None`...but, in practice, I'm fairly sure that this just
            // means that it *will not return `Ready`* until there are no data
            // frames left, which is fine for us here, because we `now_or_never`
            // it.
            body.trailers = body.inner.trailers().now_or_never();
        } else {
            // Okay, `poll_data` has returned `None`, so there are no data
            // frames left. Let's see if there's trailers...
            body.trailers = Some(body.inner.trailers().await);
        }

        http::Response::from_parts(parts, body)
    }

    fn no_trailers(rsp: http::Response<B>) -> http::Response<Self> {
        rsp.map(|inner| Self {
            inner,
            first_data: None,
            trailers: None,
        })
    }
}

impl<B> Body for WithTrailers<B>
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
        let this = self.get_mut();
        if let Some(first_data) = this.first_data.take() {
            return Poll::Ready(Some(first_data));
        }

        Pin::new(&mut this.inner).poll_data(cx)
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<http::HeaderMap>, Self::Error>> {
        let this = self.get_mut();
        if let Some(trailers) = this.trailers.take() {
            return Poll::Ready(trailers);
        }

        Pin::new(&mut this.inner).poll_trailers(cx)
    }

    #[inline]
    fn is_end_stream(&self) -> bool {
        self.first_data.is_none() && self.trailers.is_none() && self.inner.is_end_stream()
    }

    #[inline]
    fn size_hint(&self) -> http_body::SizeHint {
        use bytes::Buf;

        let mut hint = self.inner.size_hint();
        // If we're holding onto a chunk of data, add its length to the inner
        // `Body`'s size hint.
        if let Some(Ok(chunk)) = self.first_data.as_ref() {
            let buffered = chunk.remaining() as u64;
            if let Some(upper) = hint.upper() {
                hint.set_upper(upper + buffered);
            }
            hint.set_lower(hint.lower() + buffered);
        }

        hint
    }
}
