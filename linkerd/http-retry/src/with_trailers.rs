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
    first_data: Option<B::Data>,
    trailers: Option<http::HeaderMap>,
}

pub type WithTrailersFuture<B, E> = Either<
    futures::future::Ready<Result<http::Response<WithTrailers<B>>, E>>,
    Pin<Box<dyn Future<Output = Result<http::Response<WithTrailers<B>>, E>> + Send + 'static>>,
>;

// === impl WithTrailers ===

impl<B: Body> WithTrailers<B> {
    pub fn trailers(&self) -> Option<&http::HeaderMap> {
        self.trailers.as_ref()
    }

    pub fn map_response<E>(rsp: http::Response<B>) -> WithTrailersFuture<B, E>
    where
        B: Send + Unpin + 'static,
        B::Data: Send + Unpin + 'static,
        E: From<B::Error> + 'static,
    {
        use http::Version;

        // If the response isn't an HTTP version that has trailers, skip trying
        // to read a trailers frame.
        if let Version::HTTP_09 | Version::HTTP_10 | Version::HTTP_11 = rsp.version() {
            return Either::Left(future::ready(Ok(Self::no_trailers(rsp))));
        }

        // If the response doesn't have a body stream, also skip trying to read
        // a trailers frame.
        if rsp.is_end_stream() {
            return Either::Left(future::ready(Ok(Self::no_trailers(rsp))));
        }

        // Otherwise, return a future that tries to read the next frame.
        Either::Right(Box::pin(Self::read_response(rsp)))
    }

    async fn read_response<E>(rsp: http::Response<B>) -> Result<http::Response<Self>, E>
    where
        B: Send + Unpin,
        B::Data: Send + Unpin,
        E: From<B::Error>,
    {
        let (parts, body) = rsp.into_parts();
        let mut body = Self {
            inner: body,
            first_data: None,
            trailers: None,
        };

        if let Some(data) = body.inner.data().await {
            // body has data; stop waiting for trailers
            body.first_data = Some(data?);

            // peek to see if there's immediately a trailers frame, and grab
            // it if so. otherwise, bail.
            if let Some(trailers) = body.inner.trailers().now_or_never() {
                body.trailers = trailers?;
            }

            return Ok(http::Response::from_parts(parts, body));
        }

        // okay, let's see if there's trailers...
        body.trailers = body.inner.trailers().await?;

        Ok(http::Response::from_parts(parts, body))
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
{
    type Data = B::Data;
    type Error = B::Error;

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        let this = self.get_mut();
        if let Some(first_data) = this.first_data.take() {
            return Poll::Ready(Some(Ok(first_data)));
        }

        Pin::new(&mut this.inner).poll_data(cx)
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<http::HeaderMap>, Self::Error>> {
        let this = self.get_mut();
        if let Some(trailers) = this.trailers.take() {
            return Poll::Ready(Ok(Some(trailers)));
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
        if let Some(chunk) = self.first_data.as_ref() {
            let buffered = chunk.remaining() as u64;
            if let Some(upper) = hint.upper() {
                hint.set_upper(upper + buffered);
            }
            hint.set_lower(hint.lower() + buffered);
        }

        hint
    }
}
