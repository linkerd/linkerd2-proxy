use futures::FutureExt;
use http_body::Body as HttpBody;
use linkerd_error::Error;
use linkerd_stack as svc;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

/// A service that wraps HTTP responses to allow peeking at the stream's
/// trailers, if the first frame in the response stream is a `TRAILERS` frame.
#[derive(Clone)]
pub struct WithTrailers<S> {
    inner: S,
}

/// An HTTP body that allows inspecting the body's trailers, if a `TRAILERS`
/// frame was the first frame after the initial headers frame.
///
/// If the first frame of the body stream was *not* a `TRAILERS` frame, this
/// behaves identically to a normal body.
pub struct Body<B: HttpBody> {
    inner: B,
    first_data: Option<B::Data>,
    trailers: Option<http::HeaderMap>,
}

/// This has its own `Layer` type (rather than using `layer::Mk`) so that the
/// layer type doesn't include the service type's name.
#[derive(Clone, Debug, Default)]
pub struct Layer(());

// === impl Layer ===

impl<S> svc::layer::Layer<S> for Layer {
    type Service = WithTrailers<S>;
    fn layer(&self, inner: S) -> Self::Service {
        WithTrailers { inner }
    }
}

// === impl WithTrailers ===

impl<S, Req, RspBody> svc::Service<Req> for WithTrailers<S>
where
    S: svc::Service<Req, Response = http::Response<RspBody>>,
    S::Future: Send + 'static,
    RspBody: HttpBody + Send + Unpin,
    RspBody::Data: Send + Unpin,
    Error: From<S::Error> + From<RspBody::Error>,
{
    type Response = http::Response<Body<RspBody>>;
    type Error = Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let rsp = self.inner.call(req);
        Box::pin(async move {
            let (parts, body) = rsp.await?.into_parts();
            let mut body = Body {
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
        })
    }
}

// === impl WithTrailersBody ===

impl<B: HttpBody> Body<B> {
    pub fn trailers(&self) -> Option<&http::HeaderMap> {
        self.trailers.as_ref()
    }
}

impl<B> HttpBody for Body<B>
where
    B: HttpBody + Unpin,
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
