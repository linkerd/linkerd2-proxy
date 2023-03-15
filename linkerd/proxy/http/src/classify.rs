use futures::{prelude::*, ready};
use linkerd_error::Error;
use linkerd_stack::{ExtractParam, NewService};
use pin_project::pin_project;
use std::{
    future::Future,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::sync::mpsc;

pub use linkerd_http_classify::*;

pub struct NewClassifyChannel<C: Classify, X, N> {
    inner: N,
    extract: X,
    tx: mpsc::Sender<C::Class>,
    _marker: PhantomData<fn() -> C>,
}

/// A HTTP `Service` that applies a [`super::Classify`] to each request and
/// sends each request's class on a channel.
pub struct ClassifyChannel<C: Classify, S> {
    inner: S,
    classify: C,
    tx: mpsc::Sender<C::Class>,
}

#[pin_project]
pub struct ResponseFuture<C: ClassifyResponse, B, F> {
    #[pin]
    inner: F,
    classify: Option<C>,
    tx: mpsc::Sender<C::Class>,
    _marker: PhantomData<fn() -> B>,
}

#[pin_project]
pub struct ResponseBody<C: ClassifyEos, B> {
    #[pin]
    inner: B,
    classify: Option<C>,
    tx: mpsc::Sender<C::Class>,
}

impl<T, C, X, N> NewService<T> for NewClassifyChannel<C, X, N>
where
    C: Classify,
    X: ExtractParam<C, T>,
    N: NewService<T>,
{
    type Service = ClassifyChannel<C, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        ClassifyChannel {
            classify: self.extract.extract_param(&target),
            inner: self.inner.new_service(target),
            tx: self.tx.clone(),
        }
    }
}

impl<C, S, ReqB, RspB> tower::Service<http::Request<ReqB>> for ClassifyChannel<C, S>
where
    C: Classify,
    S: tower::Service<http::Request<ReqB>, Response = http::Response<RspB>, Error = Error>,
{
    type Response = http::Response<ResponseBody<C::ClassifyEos, RspB>>;
    type Error = Error;
    type Future = ResponseFuture<C::ClassifyResponse, RspB, S::Future>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<ReqB>) -> Self::Future {
        let classify = self.classify.classify(&req);
        ResponseFuture {
            classify: Some(classify),
            tx: self.tx.clone(),
            inner: self.inner.call(req),
            _marker: PhantomData,
        }
    }
}

impl<C, B, F> Future for ResponseFuture<C, B, F>
where
    C: ClassifyResponse,
    F: TryFuture<Ok = http::Response<B>, Error = Error>,
{
    type Output = Result<http::Response<ResponseBody<C::ClassifyEos, B>>, Error>;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        match ready!(this.inner.try_poll(cx)) {
            Ok(rsp) => {
                let classify = this.classify.take().map(|c| c.start(&rsp));
                Poll::Ready(Ok(rsp.map(|inner| ResponseBody {
                    inner,
                    classify,
                    tx: this.tx.clone(),
                })))
            }
            Err(e) => {
                if let Some(classify) = this.classify.take() {
                    let _ = this.tx.try_send(classify.error(&e));
                }
                Poll::Ready(Err(e))
            }
        }
    }
}

impl<C, B> hyper::body::HttpBody for ResponseBody<C, B>
where
    C: ClassifyEos + Unpin,
    B: hyper::body::HttpBody<Error = Error>,
{
    type Data = B::Data;
    type Error = B::Error;

    #[inline]
    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        let this = self.project();
        match ready!(this.inner.poll_data(cx)) {
            None => Poll::Ready(None),
            Some(Ok(data)) => Poll::Ready(Some(Ok(data))),
            Some(Err(e)) => {
                if let Some(classify) = this.classify.take() {
                    let _ = this.tx.try_send(classify.error(&e));
                }
                Poll::Ready(Some(Err(e)))
            }
        }
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<http::HeaderMap>, Self::Error>> {
        let this = self.project();
        match ready!(this.inner.poll_trailers(cx)) {
            Ok(trls) => {
                if let Some(classify) = this.classify.take() {
                    let _ = this.tx.try_send(classify.eos(trls.as_ref()));
                }
                Poll::Ready(Ok(trls))
            }
            Err(e) => {
                if let Some(classify) = this.classify.take() {
                    let _ = this.tx.try_send(classify.error(&e));
                }
                Poll::Ready(Err(e))
            }
        }
    }

    #[inline]
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    #[inline]
    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}
