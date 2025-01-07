use futures::{
    future::{self, Either},
    FutureExt, TryFutureExt,
};
use http_body::{Body, Frame};
use linkerd_http_box::BoxBody;
use linkerd_stack::Service;
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
pub struct PeekTrailersBody<B: Body = BoxBody> {
    inner: B,
    /// The first frame received from the inner body, or an error that
    /// occurred while polling for data.
    ///
    /// If this is `None`, then the body has completed without any frames.
    frame: Option<Result<Frame<B::Data>, B::Error>>,
}

pub type WithPeekTrailersBody<B> = Either<
    futures::future::Ready<http::Response<PeekTrailersBody<B>>>,
    Pin<Box<dyn Future<Output = http::Response<PeekTrailersBody<B>>> + Send + 'static>>,
>;

#[derive(Clone, Debug)]
pub struct ResponseWithPeekTrailers<S>(pub(crate) S);

// === impl WithTrailers ===

impl<B: Body> PeekTrailersBody<B> {
    pub fn peek_trailers(&self) -> Option<&http::HeaderMap> {
        self.frame
            .as_ref()
            .and_then(|f| f.as_ref().ok())
            .and_then(Frame::trailers_ref)
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
            return Either::Left(future::ready(Self::no_trailers(rsp)));
        }

        // If the response doesn't have a body stream, also skip trying to read
        // a trailers frame.
        if rsp.is_end_stream() {
            tracing::debug!("Skipping trailers for empty body");
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
        use http_body_util::BodyExt;

        let (parts, mut body) = rsp.into_parts();

        tracing::debug!("Buffering first body frame");
        let frame = body.frame().await;

        if let Some(Ok(frame)) = &frame {
            if frame.trailers_ref().is_some() {
                tracing::debug!("Buffered trailers frame");
            }
        }

        let body = Self { inner: body, frame };

        http::Response::from_parts(parts, body)
    }

    fn no_trailers(rsp: http::Response<B>) -> http::Response<Self> {
        rsp.map(|inner| Self { inner, frame: None })
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

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();

        if let frame @ Some(_) = this.frame.take() {
            return Poll::Ready(frame);
        }

        Pin::new(&mut this.inner).poll_frame(cx)
    }

    #[inline]
    fn is_end_stream(&self) -> bool {
        self.frame.is_none() && self.inner.is_end_stream()
    }

    #[inline]
    fn size_hint(&self) -> http_body::SizeHint {
        use bytes::Buf;

        let mut hint = self.inner.size_hint();
        // If we're holding onto a chunk of data, add its length to the inner
        // `Body`'s size hint.
        if let Some(Ok(frame)) = self.frame.as_ref() {
            if let Some(chunk) = frame.data_ref() {
                let buffered = chunk.remaining() as u64;
                if let Some(upper) = hint.upper() {
                    hint.set_upper(upper + buffered);
                }
                hint.set_lower(hint.lower() + buffered);
            }
        }

        hint
    }
}

// === impl ResponseWithTrailers ===

type WrapTrailersFuture<E> = future::Map<
    WithPeekTrailersBody<BoxBody>,
    fn(http::Response<PeekTrailersBody>) -> Result<http::Response<PeekTrailersBody>, E>,
>;

impl<Req, S> Service<Req> for ResponseWithPeekTrailers<S>
where
    S: Service<Req, Response = http::Response<BoxBody>>,
{
    type Response = http::Response<PeekTrailersBody>;
    type Error = S::Error;
    type Future = future::AndThen<
        S::Future,
        WrapTrailersFuture<S::Error>,
        fn(http::Response<BoxBody>) -> WrapTrailersFuture<S::Error>,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    #[inline]
    fn call(&mut self, req: Req) -> Self::Future {
        self.0
            .call(req)
            .and_then(|rsp| PeekTrailersBody::map_response(rsp).map(Ok))
    }
}
