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
    /// The inner body type.
    inner: B,
    /// The first frame received from the inner body.
    ///
    /// This may be a DATA frame or a TRAILERS frame.
    peek: Option<Result<Frame<B::Data>, B::Error>>,
}

pub type WithPeekTrailersBody<B> = Either<
    futures::future::Ready<http::Response<PeekTrailersBody<B>>>,
    Pin<Box<dyn Future<Output = http::Response<PeekTrailersBody<B>>> + Send + 'static>>,
>;

#[derive(Clone, Debug)]
pub struct ResponseWithPeekTrailers<S>(pub(crate) S);

// === impl WithTrailers ===

impl<B: Body> PeekTrailersBody<B> {
    /// Returns a reference to the body's trailers.
    pub fn peek_trailers(&self) -> Option<&http::HeaderMap> {
        match self.peek {
            Some(Ok(frame)) => frame.trailers_ref(),
            _ => None,
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
        let (parts, mut body) = rsp.into_parts();

        use http_body_util::BodyExt;
        tracing::debug!("Buffering first frame");
        let peek = body.frame().await;

        http::Response::from_parts(parts, Self { inner: body, peek })
    }

    /// Returns a response with a disabled [`PeekTrailersBody`].
    fn no_trailers(rsp: http::Response<B>) -> http::Response<Self> {
        rsp.map(|inner| Self {
            inner,
            peek: None, // Refrain from awaiting the first frame.
        })
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
        let Self { mut inner, peek } = self.get_mut();

        // If a frame is buffered, yield it.
        if let frame @ Some(_) = peek.take() {
            return Poll::Ready(frame);
        }

        Pin::new(&mut inner).poll_frame(cx)
    }

    #[inline]
    fn is_end_stream(&self) -> bool {
        let Self { inner, peek } = self;

        // Is a frame buffered? Has inner body has reached its end?
        peek.is_none() && inner.is_end_stream()
    }

    #[inline]
    fn size_hint(&self) -> http_body::SizeHint {
        use bytes::Buf;

        let Self { inner, peek } = self;
        let mut hint = inner.size_hint();

        // If we're holding onto a chunk of data, add its length to the inner `Body`'s size hint.
        if let Some(Ok(frame)) = peek.as_ref() {
            if let Some(buffered) = frame.data_ref().map(|data| data.remaining() as u64) {
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
