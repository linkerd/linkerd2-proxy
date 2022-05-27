use http_body::Body as HttpBody;
use linkerd_error::Error;
use linkerd_stack as svc;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

#[derive(Clone)]
pub struct WithTrailers<S> {
    inner: S,
}
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
    S::Error: Into<Error>,
    RspBody: HttpBody + Send + Unpin,
    RspBody::Error: Into<Error>,
    RspBody::Data: Send + Unpin,
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
            let (parts, body) = rsp.await.map_err(Into::into)?.into_parts();
            let mut body = Body {
                inner: body,
                first_data: None,
                trailers: None,
            };

            if let Some(data) = body.inner.data().await {
                // body has data; stop waiting for trailers
                // TODO(eliza): we could maybe do a last-gasp "is the next frame
                // trailers"? check here, but that would add additional
                // latency...
                body.first_data = Some(data.map_err(Into::into)?);
                return Ok(http::Response::from_parts(parts, body));
            }

            // okay, let's see if there's trailers...
            body.trailers = body.inner.trailers().await.map_err(Into::into)?;

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

            hint.set_lower(hint.lower() + buffered);
            if let Some(upper) = hint.upper() {
                hint.set_upper(upper + buffered);
            }
        }

        hint
    }
}
