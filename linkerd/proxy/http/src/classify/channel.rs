use super::{ClassifyEos, ClassifyResponse};
use futures::{prelude::*, ready};
use linkerd_error::Error;
use linkerd_stack::{layer, ExtractParam, NewService, Service};
use pin_project::pin_project;
use std::{
    future::Future,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::sync::mpsc;

#[derive(Debug)]
pub struct NewBroadcastClassification<C, X, N> {
    inner: N,
    extract: X,
    _marker: PhantomData<fn() -> C>,
}

/// A HTTP `Service` that applies a [`super::Classify`] to each request and
/// broadcasts the result to an [`Rx`] inserted to inner requests and outer
/// responses..
#[derive(Debug)]
pub struct BroadcastClassification<C: ClassifyResponse, S> {
    inner: S,
    tx: mpsc::Sender<C::Class>,
    _marker: PhantomData<fn() -> C>,
}

#[derive(Clone, Debug)]
pub struct Tx<C>(pub mpsc::Sender<C>);

#[pin_project]
pub struct ResponseFuture<C: ClassifyResponse, B, F> {
    #[pin]
    inner: F,
    state: Option<State<C, C::Class>>,
    _marker: PhantomData<fn() -> B>,
}

#[pin_project]
pub struct ResponseBody<C: ClassifyEos, B> {
    #[pin]
    inner: B,
    state: Option<State<C, C::Class>>,
}

#[derive(Debug)]
struct State<C, T> {
    classify: C,
    tx: mpsc::Sender<T>,
}

// === impl NewBroadcastClassification ===

impl<C, X: Clone, N> NewBroadcastClassification<C, X, N> {
    pub fn new(extract: X, inner: N) -> Self {
        Self {
            inner,
            extract,
            _marker: PhantomData,
        }
    }

    pub fn layer_via(extract: X) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(extract.clone(), inner))
    }
}

impl<C, N> NewBroadcastClassification<C, (), N> {
    pub fn layer() -> impl layer::Layer<N, Service = Self> + Clone {
        Self::layer_via(())
    }
}

impl<T, C, X, N> NewService<T> for NewBroadcastClassification<C, X, N>
where
    C: ClassifyResponse,
    X: ExtractParam<Tx<C::Class>, T>,
    N: NewService<T>,
{
    type Service = BroadcastClassification<C, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let Tx(tx) = self.extract.extract_param(&target);
        let inner = self.inner.new_service(target);
        BroadcastClassification::new(tx, inner)
    }
}

impl<C, X: Clone, N: Clone> Clone for NewBroadcastClassification<C, X, N> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            extract: self.extract.clone(),
            _marker: PhantomData,
        }
    }
}

// === impl BroadcastClassification ===

impl<C: ClassifyResponse, S> BroadcastClassification<C, S> {
    pub fn new(tx: mpsc::Sender<C::Class>, inner: S) -> Self {
        Self {
            inner,
            tx,
            _marker: PhantomData,
        }
    }
}

impl<C, S, ReqB, RspB> Service<http::Request<ReqB>> for BroadcastClassification<C, S>
where
    C: ClassifyResponse,
    S: Service<http::Request<ReqB>, Response = http::Response<RspB>, Error = Error>,
{
    type Response = http::Response<ResponseBody<C::ClassifyEos, RspB>>;
    type Error = Error;
    type Future = ResponseFuture<C, RspB, S::Future>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<ReqB>) -> Self::Future {
        let tx = self.tx.clone();
        let state = req
            .extensions()
            .get::<C>()
            .cloned()
            .map(|classify| State { classify, tx });

        let inner = self.inner.call(req);
        ResponseFuture {
            inner,
            state,
            _marker: PhantomData,
        }
    }
}

impl<C, S> Clone for BroadcastClassification<C, S>
where
    C: ClassifyResponse + Clone,
    S: Clone,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            tx: self.tx.clone(),
            _marker: PhantomData,
        }
    }
}

// === impl ResponseFuture ===

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
                let state = this.state.take().map(|State { classify, tx }| {
                    let classify = classify.start(&rsp);
                    State { classify, tx }
                });
                Poll::Ready(Ok(rsp.map(|inner| ResponseBody { inner, state })))
            }

            Err(e) => {
                if let Some(State { classify, tx }) = this.state.take() {
                    let class = classify.error(&e);
                    let _ = tx.try_send(class);
                }
                Poll::Ready(Err(e))
            }
        }
    }
}

// === impl ResponseBody ===

impl<C, B> hyper::body::HttpBody for ResponseBody<C, B>
where
    C: ClassifyEos + Unpin,
    B: hyper::body::HttpBody<Error = Error>,
{
    type Data = B::Data;
    type Error = B::Error;

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        let this = self.project();
        match ready!(this.inner.poll_data(cx)) {
            None => Poll::Ready(None),
            Some(Ok(data)) => Poll::Ready(Some(Ok(data))),
            Some(Err(e)) => {
                if let Some(State { classify, tx }) = this.state.take() {
                    let _ = tx.try_send(classify.error(&e));
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
                if let Some(State { classify, tx }) = this.state.take() {
                    let _ = tx.try_send(classify.eos(trls.as_ref()));
                }
                Poll::Ready(Ok(trls))
            }
            Err(e) => {
                if let Some(State { classify, tx }) = this.state.take() {
                    let _ = tx.try_send(classify.error(&e));
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
