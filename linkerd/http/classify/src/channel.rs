use super::{ClassifyEos, ClassifyResponse};
use futures::{prelude::*, ready};
use http_body::Frame;
use linkerd_error::Error;
use linkerd_stack::Service;
use pin_project::{pin_project, pinned_drop};
use std::{
    fmt::Debug,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::sync::mpsc;

/// A HTTP `Service` that applies a [`ClassifyResponse`] to each response, and
/// broadcasts the classification over a [`mpsc`] channel.
#[derive(Debug)]
pub struct BroadcastClassification<C: ClassifyResponse, S> {
    inner: S,
    tx: mpsc::Sender<C::Class>,
    _marker: PhantomData<fn() -> C>,
}

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
        match ready!(this.inner.poll_frame(cx)) {
            None => {
                // Classify the stream if it has reached a `None`.
                if let Some(State { classify, tx }) = this.state.take() {
                    let _ = tx.try_send(classify.eos(None));
                }
                Poll::Ready(None)
            }
            Some(Ok(data)) => {
                // Classify the stream if this is a trailers frame.
                if let trls @ Some(_) = data.trailers_ref() {
                    if let Some(State { classify, tx }) = this.state.take() {
                        let _ = tx.try_send(classify.eos(trls));
                    }
                }
                Poll::Ready(Some(Ok(data)))
            }
            Some(Err(e)) => {
                // Classify the stream if an error has been encountered.
                if let Some(State { classify, tx }) = this.state.take() {
                    let _ = tx.try_send(classify.error(&e));
                }
                Poll::Ready(Some(Err(e)))
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
