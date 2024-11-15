use super::{ClassifyEos, ClassifyResponse};
use futures::{prelude::*, ready};
use http_body::Frame;
use linkerd_error::Error;
use linkerd_stack::{layer, ExtractParam, NewService, Service};
use pin_project::{pin_project, pinned_drop};
use std::{
    fmt::Debug,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::sync::mpsc;

/// Constructs new [`BroadcastClassification`] services.
///
/// `X` is an [`ExtractParam`] implementation that extracts a [`Tx`] from each
/// target. The [`Tx`] is used to broadcast the classification of each response
/// from the constructed [`BroadcastClassification`] service.
#[derive(Debug)]
pub struct NewBroadcastClassification<C, X, N> {
    inner: N,
    extract: X,
    _marker: PhantomData<fn() -> C>,
}

/// A HTTP `Service` that applies a [`ClassifyResponse`] to each response, and
/// broadcasts the classification over a [`mpsc`] channel.
#[derive(Debug)]
pub struct BroadcastClassification<C: ClassifyResponse, S> {
    inner: S,
    tx: mpsc::Sender<C::Class>,
    _marker: PhantomData<fn() -> C>,
}

/// A handle to a [`mpsc`] channel over which response classifications are
/// broadcasted.
///
/// This is extracted from a target value by [`NewBroadcastClassification`] when
/// constructing a [`BroadcastClassification`] service.
#[derive(Clone, Debug)]
pub struct Tx<C>(pub mpsc::Sender<C>);

#[pin_project]
pub struct ResponseFuture<C: ClassifyResponse, B, F> {
    #[pin]
    inner: F,
    state: Option<State<C, C::Class>>,
    _marker: PhantomData<fn() -> B>,
}

#[pin_project(PinnedDrop)]
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

    /// Returns a [`layer::Layer`] that constructs `NewBroadcastClassification`
    /// [`NewService`]s, using the provided [`ExtractParam`] implementation to
    /// extract a classification [`Tx`] from the target.
    pub fn layer_via(extract: X) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(extract.clone(), inner))
    }
}

impl<C, N> NewBroadcastClassification<C, (), N> {
    /// Returns a [`layer::Layer`] that constructs `NewBroadcastClassification`
    /// [`NewService`]s when the target type implements
    /// [`linkerd_stack::Param`]`<`[`Tx`]`>`.
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
    C: ClassifyResponse + Debug,
    C::Class: Debug,
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
        tracing::debug!(?state);

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

impl<C, B> http_body::Body for ResponseBody<C, B>
where
    C: ClassifyEos + Unpin,
    B: http_body::Body<Error = Error>,
{
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.project();

        let frame = ready!(this.inner.poll_frame(cx));
        self.classify_frame(&frame);

        Poll::Ready(frame)
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

impl<C, B> ResponseBody<C, B>
where
    C: ClassifyEos,
    B: http_body::Body,
{
    /// Classifies the response body, if the frame yielded represents the end of a stream.
    fn classify_frame(
        self: Pin<&mut Self>,
        frame: &Option<Result<Frame<<B as http_body::Body>::Data>, <B as http_body::Body>::Error>>,
    ) {
        let this = self.project();
        let state = this.state;

        match frame {
            // We have reached the end of the stream if we have received a TRAILERS frame.
            Some(Ok(frame)) => {
                if let Some(trailers) = frame.trailers_ref() {
                    if let Some(State { classify, tx }) = state.take() {
                        let c = classify.eos(Some(trailers));
                        let _ = tx.try_send(c).ok();
                    }
                }
            }
            // We have reached the end of the stream if a `None` was yielded.
            None => {
                if let Some(State { classify, tx }) = state.take() {
                    let c = classify.eos(None);
                    let _ = tx.try_send(c).ok();
                }
            }
            // We have reached the end of the stream if an error has occurred.
            Some(Err(error)) => {
                if let Some(State { classify, tx }) = state.take() {
                    let c = classify.error(&error);
                    let _ = tx.try_send(c).ok();
                }
            }
        }
    }
}

#[pinned_drop]
impl<C: ClassifyEos, B> PinnedDrop for ResponseBody<C, B> {
    fn drop(self: Pin<&mut Self>) {
        tracing::debug!("dropping ResponseBody");
        if let Some(State { classify, tx }) = self.project().state.take() {
            tracing::debug!("sending EOS to classify");
            let _ = tx.try_send(classify.eos(None));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linkerd_http_box::BoxBody;
    use tokio::time;
    use tokio_test::assert_ready;
    use tower_test::mock;

    #[derive(Debug, Clone, Eq, PartialEq)]
    struct TestClass;

    impl ClassifyResponse for TestClass {
        type Class = TestClass;
        type ClassifyEos = TestClass;

        fn start<B>(self, _: &http::Response<B>) -> TestClass {
            TestClass
        }

        fn error(self, _: &Error) -> Self::Class {
            TestClass
        }
    }

    impl ClassifyEos for TestClass {
        type Class = TestClass;

        fn eos(self, _: Option<&http::HeaderMap>) -> Self::Class {
            TestClass
        }

        fn error(self, _: &Error) -> Self::Class {
            TestClass
        }
    }

    #[tokio::test]
    async fn broadcasts() {
        let _trace = linkerd_tracing::test::with_default_filter("linkerd=debug");

        let (rsps_tx, mut rsps) = mpsc::channel(1);
        let (inner, mut mock) = mock::pair::<http::Request<BoxBody>, http::Response<BoxBody>>();
        let mut svc =
            mock::Spawn::new(BroadcastClassification::<TestClass, _>::new(rsps_tx, inner));

        mock.allow(1);
        assert_ready!(svc.poll_ready()).expect("ok");

        rsps.try_recv()
            .expect_err("should not have received a response");
        let req = http::Request::builder()
            .extension(TestClass)
            .body(BoxBody::default())
            .unwrap();
        let (rsp, _) = tokio::join! {
            svc.call(req).map(|res| res.expect("must not fail")),
            mock.next_request().map(|req| {
                let (_, tx) = req.expect("request");
                tx.send_response(http::Response::default());
                tracing::debug!("sent response");
            }),
        };
        // Consume the response body and trailers to drive classification.
        drop(rsp);
        time::timeout(time::Duration::from_secs(1), rsps.recv())
            .await
            .expect("should have received a response");
    }
}
