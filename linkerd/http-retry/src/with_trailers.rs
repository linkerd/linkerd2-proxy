use http_body::Body;
use linkerd_error::Error;
use linkerd_stack::Service;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

pub struct WithTrailersSvc<S> {
    inner: S,
}

pub struct WithTrailersBody<B: Body> {
    inner: B,
    first_data: Option<B::Data>,
    trailers: Option<http::HeaderMap>,
}

impl<S, Req, RspBody> Service<Req> for WithTrailersSvc<S>
where
    S: Service<Req, Response = http::Response<RspBody>>,
    S::Future: Send + 'static,
    S::Error: Into<Error>,
    RspBody: Body + Send + Unpin,
    RspBody::Error: Into<Error>,
    RspBody::Data: Send,
{
    type Response = http::Response<WithTrailersBody<RspBody>>;
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
            let mut body = WithTrailersBody {
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
