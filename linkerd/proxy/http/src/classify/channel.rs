use super::{Classify, ClassifyEos, ClassifyResponse};
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
use tokio::sync::watch;

#[derive(Debug)]
pub struct NewInsertClassifyRx<C, X, N> {
    inner: N,
    extract: X,
    _marker: PhantomData<fn() -> C>,
}

pub type Tx<C> = watch::Sender<Poll<C>>;

#[derive(Clone, Debug)]
pub struct Rx<C>(watch::Receiver<Poll<C>>);

/// A HTTP `Service` that applies a [`super::Classify`] to each request and
/// broadcasts the result to an [`Rx`] inserted to inner requests and outer
/// responses..
#[derive(Debug)]
pub struct InsertClassifyRx<C: Classify, S> {
    inner: S,
    classify: C,
}

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
    tx: Tx<T>,
    rx: Rx<T>,
}

// === impl NewClassifyChannel ===

impl<C, X: Clone, N> NewInsertClassifyRx<C, X, N> {
    pub fn new(extract: X, inner: N) -> Self {
        Self {
            inner,
            extract,
            _marker: PhantomData,
        }
    }

    #[inline]
    pub fn layer_via(extract: X) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(extract.clone(), inner))
    }
}

impl<C, N> NewInsertClassifyRx<C, (), N> {
    #[inline]
    pub fn layer() -> impl layer::Layer<N, Service = Self> + Clone {
        Self::layer_via(())
    }
}

impl<T, C, X, N> NewService<T> for NewInsertClassifyRx<C, X, N>
where
    C: Classify,
    X: ExtractParam<C, T>,
    N: NewService<T>,
{
    type Service = InsertClassifyRx<C, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let classify = self.extract.extract_param(&target);
        let inner = self.inner.new_service(target);
        InsertClassifyRx::new(classify, inner)
    }
}

impl<C, X: Clone, N: Clone> Clone for NewInsertClassifyRx<C, X, N> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            extract: self.extract.clone(),
            _marker: PhantomData,
        }
    }
}

// === impl ClassifyChannel ===

impl<C: Classify, S> InsertClassifyRx<C, S> {
    pub fn new(classify: C, inner: S) -> Self {
        Self { inner, classify }
    }
}

impl<C, S, ReqB, RspB> Service<http::Request<ReqB>> for InsertClassifyRx<C, S>
where
    C: Classify,
    S: Service<http::Request<ReqB>, Response = http::Response<RspB>, Error = Error>,
{
    type Response = http::Response<ResponseBody<C::ClassifyEos, RspB>>;
    type Error = Error;
    type Future = ResponseFuture<C::ClassifyResponse, RspB, S::Future>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<ReqB>) -> Self::Future {
        let classify = self.classify.classify(&req);

        // This channel is used to communicate from this module to any other
        // module that observes the receiver inserted into the request and
        // response.
        let (tx, rx) = watch::channel(Poll::Pending);
        // Advertise the receiver to inner modules that observe the request.
        req.extensions_mut().insert(Rx(rx.clone()));
        let state = State {
            tx,
            rx: Rx(rx),
            classify,
        };

        let inner = self.inner.call(req);
        ResponseFuture {
            inner,
            state: Some(state),
            _marker: PhantomData,
        }
    }
}

impl<C, S> Clone for InsertClassifyRx<C, S>
where
    C: Classify + Clone,
    S: Clone,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            classify: self.classify.clone(),
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
        let res = ready!(this.inner.try_poll(cx));
        let State { classify, tx, rx } = this.state.take().expect("state must be set");
        match res {
            Ok(mut rsp) => {
                let classify = classify.start(&rsp);

                // Advertise the receiver to outer modules that observe the
                // response.
                rsp.extensions_mut().insert(rx.clone());

                Poll::Ready(Ok(rsp.map(|inner| ResponseBody {
                    inner,
                    state: Some(State { classify, tx, rx }),
                })))
            }

            Err(e) => {
                let class = classify.error(&e);
                let _ = tx.send(Poll::Ready(class));
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
                let State { classify, tx, .. } = this.state.take().expect("state must be set");
                let _ = tx.send(Poll::Ready(classify.error(&e)));
                Poll::Ready(Some(Err(e)))
            }
        }
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<http::HeaderMap>, Self::Error>> {
        let this = self.project();
        let res = ready!(this.inner.poll_trailers(cx));
        let State { classify, tx, .. } = this.state.take().expect("state must be set");
        match res {
            Ok(trls) => {
                let _ = tx.send(Poll::Ready(classify.eos(trls.as_ref())));
                Poll::Ready(Ok(trls))
            }
            Err(e) => {
                let _ = tx.send(Poll::Ready(classify.error(&e)));
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

// === impl Rx ===

impl<C: Clone> Rx<C> {
    pub async fn recv(&mut self) -> Result<C, watch::error::RecvError> {
        loop {
            if let Poll::Ready(ref c) = *self.0.borrow_and_update() {
                return Ok(c.clone());
            }
            self.0.changed().await?;
        }
    }
}
