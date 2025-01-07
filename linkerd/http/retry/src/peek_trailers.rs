use futures::{
    future::{self, Either},
    FutureExt, TryFutureExt,
};
use http_body::Body;
use linkerd_http_box::BoxBody;
use linkerd_stack::Service;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

/// An HTTP body that allows inspecting the body's trailers.
///
/// The body's trailers may be peeked with [`PeekTrailersBody::peek_trailers()`].
///
/// Trailers may only be peeked if the inner body immediately yields a TRAILERS frame. If the first
/// frame of the body stream was *not* a `TRAILERS` frame, this behaves identically to a normal
/// body.
pub enum PeekTrailersBody<B: Body = BoxBody> {
    /// The trailers are not available to be inspected.
    Passthru {
        /// The inner body.
        inner: B,
        /// The first DATA frame received from the inner body, or an error that
        /// occurred while polling for data.
        ///
        /// If this is `None`, then the body has completed without any DATA frames.
        first_data: Option<Result<B::Data, B::Error>>,
    },
    /// The trailers have been peeked.
    ///
    /// This variant applies if the inner body's first frame was a `TRAILERS` frame.
    Peek {
        /// The inner body's trailers.
        trailers: Option<Result<Option<http::HeaderMap>, B::Error>>,
    },
}

pub type WithPeekTrailersBody<B> = Either<
    futures::future::Ready<http::Response<PeekTrailersBody<B>>>,
    Pin<Box<dyn Future<Output = http::Response<PeekTrailersBody<B>>> + Send + 'static>>,
>;

#[derive(Clone, Debug)]
pub struct ResponseWithPeekTrailers<S>(pub(crate) S);

// === impl WithTrailers ===

impl<B: Body> PeekTrailersBody<B> {
    /// Returns a reference to the trailers, if applicable.
    ///
    /// See [`PeekTrailersBody<B>`] for more information on when this returns `None`.
    pub fn peek_trailers(&self) -> Option<&http::HeaderMap> {
        match self {
            Self::Peek {
                trailers: Some(Ok(Some(ref t))),
            } => Some(t),
            Self::Passthru { .. } | Self::Peek { .. } => None,
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

        tracing::debug!("Buffering first body frame");
        if let first_data @ Some(_) = body.data().await {
            // The body has data; stop waiting for trailers.
            let body = Self::Passthru {
                inner: body,
                first_data,
            };
            return http::Response::from_parts(parts, body);
        }

        // We have confirmed that there are no data frames. Now, await the trailers.
        let trailers = body.trailers().await;
        tracing::debug!("Buffered trailers frame");
        let body = Self::Peek {
            trailers: Some(trailers),
        };
        http::Response::from_parts(parts, body)
    }

    /// Returns a response with an inert [`PeekTrailersBody<B>`].
    fn no_trailers(rsp: http::Response<B>) -> http::Response<Self> {
        rsp.map(|inner| Self::Passthru {
            inner,
            first_data: None,
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

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        match self.get_mut() {
            Self::Passthru {
                first_data,
                ref mut inner,
            } => {
                // Return the first chunk that was buffered originally.
                if let data @ Some(_) = first_data.take() {
                    return Poll::Ready(data);
                }
                // ...and then, poll the inner body.
                Pin::new(inner).poll_data(cx)
            }
            // If we have peeked the trailers, we've already polled an empty body.
            Self::Peek { .. } => Poll::Ready(None),
        }
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<http::HeaderMap>, Self::Error>> {
        match self.get_mut() {
            Self::Passthru { ref mut inner, .. } => Pin::new(inner).poll_trailers(cx),
            Self::Peek { trailers } => {
                let trailers = trailers
                    .take()
                    .expect("poll_trailers should not be called more than once");
                Poll::Ready(trailers)
            }
        }
    }

    #[inline]
    fn is_end_stream(&self) -> bool {
        match self {
            Self::Passthru { inner, first_data } => first_data.is_none() && inner.is_end_stream(),
            Self::Peek { trailers: Some(_) } => false,
            Self::Peek { trailers: None } => true,
        }
    }

    #[inline]
    fn size_hint(&self) -> http_body::SizeHint {
        use bytes::Buf;

        match self {
            Self::Passthru { inner, first_data } => {
                let mut hint = inner.size_hint();
                // If we're holding onto a chunk of data, add its length to the inner
                // `Body`'s size hint.
                if let Some(Ok(chunk)) = first_data.as_ref() {
                    let buffered = chunk.remaining() as u64;
                    if let Some(upper) = hint.upper() {
                        hint.set_upper(upper + buffered);
                    }
                    hint.set_lower(hint.lower() + buffered);
                }
                hint
            }
            Self::Peek { .. } => http_body::SizeHint::default(),
        }
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
