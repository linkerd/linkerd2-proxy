use futures::FutureExt;
use linkerd_stack::Service;
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use tokio::time;

pub struct StreamIdleTimeout<S> {
    timeout: time::Duration,
    inner: S,
}

#[pin_project::pin_project]
pub struct ResponseFuture<F> {
    timeout: time::Duration,
    sleep: Option<Pin<Box<time::Sleep>>>,

    #[pin]
    inner: F,
}

#[pin_project::pin_project]
pub struct StreamIdleBody<B> {
    timeout: time::Duration,
    sleep: Option<Pin<Box<time::Sleep>>>,
    idled_out: bool,

    #[pin]
    inner: B,
}

impl<Req, S, B> Service<Req> for StreamIdleTimeout<S>
where
    S: Service<Req, Response = http::Response<B>>,
    B: Default,
{
    type Response = http::Response<StreamIdleBody<B>>;
    type Error = S::Error;
    type Future = ResponseFuture<S::Future>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        ResponseFuture {
            inner: self.inner.call(req),
            sleep: Some(Box::pin(time::sleep(self.timeout))),
            timeout: self.timeout,
        }
    }
}

impl<F, B, E> std::future::Future for ResponseFuture<F>
where
    F: std::future::Future<Output = Result<http::Response<B>, E>>,
    B: Default,
{
    type Output = Result<http::Response<StreamIdleBody<B>>, E>;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        let sleep = this.sleep.as_mut().expect("polled after complete");
        if sleep.poll_unpin(cx).is_ready() {
            // TODO metric
            return Poll::Ready(Ok(http::Response::builder()
                .status(http::StatusCode::REQUEST_TIMEOUT)
                .body(StreamIdleBody {
                    timeout: *this.timeout,
                    sleep: None,
                    inner: B::default(),
                    idled_out: false,
                })
                .unwrap()));
        }

        let rsp = futures::ready!(this.inner.poll(cx))?;

        let sleep = this.sleep.take().expect("polled after complete");
        Poll::Ready(Ok(rsp.map(|body| StreamIdleBody {
            inner: body,
            sleep: Some(sleep),
            idled_out: false,
            timeout: *this.timeout,
        })))
    }
}

impl<B> http_body::Body for StreamIdleBody<B>
where
    B: http_body::Body,
{
    type Data = B::Data;
    type Error = B::Error;

    fn poll_data(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        let this = self.project();
        if *this.idled_out {
            return Poll::Ready(None);
        }

        match this.inner.poll_data(cx) {
            Poll::Ready(Some(Ok(data))) => {
                if let Some(sleep) = this.sleep.as_mut() {
                    sleep.as_mut().reset(time::Instant::now() + *this.timeout);
                    let poll = sleep.poll_unpin(cx);
                    assert!(poll.is_pending());
                }
                Poll::Ready(Some(Ok(data)))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(Err(e))) => {
                *this.sleep = None;
                Poll::Ready(Some(Err(e)))
            }
            Poll::Pending => {
                if let Some(sleep) = this.sleep.as_mut() {
                    if sleep.poll_unpin(cx).is_ready() {
                        *this.idled_out = true;
                        // TODO metric
                        return Poll::Ready(None);
                    }
                }
                Poll::Pending
            }
        }
    }

    fn poll_trailers(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<Option<http::HeaderMap>, Self::Error>> {
        let this = self.project();
        if *this.idled_out {
            return Poll::Ready(Ok(None));
        }

        match this.inner.poll_trailers(cx) {
            Poll::Ready(Ok(Some(trls))) => {
                if let Some(sleep) = this.sleep.as_mut() {
                    sleep.as_mut().reset(time::Instant::now() + *this.timeout);
                    let poll = sleep.poll_unpin(cx);
                    assert!(poll.is_pending());
                }
                Poll::Ready(Ok(Some(trls)))
            }
            Poll::Ready(Ok(None)) => Poll::Ready(Ok(None)),
            Poll::Ready(Err(e)) => {
                *this.sleep = None;
                Poll::Ready(Err(e))
            }
            Poll::Pending => {
                if let Some(sleep) = this.sleep.as_mut() {
                    if sleep.poll_unpin(cx).is_ready() {
                        *this.idled_out = true;
                        // TODO metric
                        return Poll::Ready(Ok(None));
                    }
                }
                Poll::Pending
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.idled_out || self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}
