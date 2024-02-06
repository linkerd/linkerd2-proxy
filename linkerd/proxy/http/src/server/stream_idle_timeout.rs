use futures::FutureExt;
use linkerd_metrics::prom;
use linkerd_stack::Service;
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use tokio::time;

/// A middleware that prevents streams from remaining idle indefinitely.
///
/// This middleware wraps HTTP response futures and bodies with an idle. If the
/// full response headers are not received within the, a 408 Request Timeout
/// response is returned. If the response headers are received, but the elapses
/// between stream updates, the stream is ended.
///
/// NOTE that this is primarily intended for HTTP/1.x streams. While there isn't
/// really any harm in using this with an HTTP/2 stream, h2 servers may not call
/// poll_data while awaiting flow control capacity. This means that the idle
/// timeout may not be activated on streams awaiting flow control window
/// capacity.
#[derive(Clone, Debug)]
pub struct StreamIdleTimeout<S> {
    metrics: Metrics,
    timeout: time::Duration,
    inner: S,
}

#[derive(Debug)]
#[pin_project::pin_project]
pub struct ResponseFuture<F> {
    metrics: Metrics,
    timeout: time::Duration,
    sleep: Option<Pin<Box<time::Sleep>>>,

    #[pin]
    inner: F,
}

#[derive(Debug)]
#[pin_project::pin_project]
pub struct StreamIdleBody<B> {
    metrics: Metrics,
    timeout: time::Duration,
    sleep: Option<Pin<Box<time::Sleep>>>,
    idled_out: bool,

    #[pin]
    inner: B,
}

#[derive(Clone, Debug)]
pub struct MetricFamilies<L> {
    counter: prom::Family<L, prom::Counter>,
}

#[derive(Clone, Debug, Default)]
pub struct Metrics {
    counter: prom::Counter,
}

// === impl MetricFamilies ===

impl<L> MetricFamilies<L>
where
    L: prom::encoding::EncodeLabelSet + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone + Send + Sync + 'static,
{
    pub fn register(registry: &mut prom::Registry) -> Self {
        let counter = prom::Family::default();
        registry.register(
            "stream_idle_timeout",
            "The number of times a stream has idled out",
            counter.clone(),
        );
        Self { counter }
    }

    pub fn metrics(&self, labels: &L) -> Metrics {
        Metrics {
            counter: self.counter.get_or_create(labels).clone(),
        }
    }
}

// === impl StreamIdleTimeout ===

impl<S> StreamIdleTimeout<S> {
    pub fn new(timeout: time::Duration, metrics: Metrics, inner: S) -> Self {
        // Roughly 30 years from now.
        const MAX_IDLE: time::Duration = time::Duration::from_secs(86400 * 365 * 30);

        // Prevents excessively aggressives.
        const MIN_IDLE: time::Duration = time::Duration::from_secs(1);

        Self {
            inner,
            metrics,
            timeout: timeout.min(MAX_IDLE).max(MIN_IDLE),
        }
    }
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

    #[inline]
    fn call(&mut self, req: Req) -> Self::Future {
        ResponseFuture {
            inner: self.inner.call(req),
            sleep: Some(Box::pin(time::sleep(self.timeout))),
            timeout: self.timeout,
            metrics: self.metrics.clone(),
        }
    }
}

// === impl ResponseFuture ===

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
            tracing::info!(timeout = ?this.timeout, "Stream idle before response headers");
            this.metrics.counter.inc();

            return Poll::Ready(Ok(http::Response::builder()
                .status(http::StatusCode::REQUEST_TIMEOUT)
                .body(StreamIdleBody {
                    timeout: *this.timeout,
                    sleep: None,
                    metrics: this.metrics.clone(),
                    inner: B::default(),
                    idled_out: false,
                })
                .unwrap()));
        }

        let rsp = futures::ready!(this.inner.poll(cx))?;

        let mut sleep = this.sleep.take().expect("polled after complete");
        sleep.as_mut().reset(time::Instant::now() + *this.timeout);

        Poll::Ready(Ok(rsp.map(|body| StreamIdleBody {
            inner: body,
            sleep: Some(sleep),
            idled_out: false,
            metrics: this.metrics.clone(),
            timeout: *this.timeout,
        })))
    }
}

// === impl StreamIdleBody ===

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
                        tracing::info!(timeout = ?this.timeout, "Stream idle awaiting data");
                        this.metrics.counter.inc();

                        *this.sleep = None;
                        *this.idled_out = true;
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
            Poll::Ready(Ok(None)) => {
                *this.sleep = None;
                Poll::Ready(Ok(None))
            }
            Poll::Ready(Err(e)) => {
                *this.sleep = None;
                Poll::Ready(Err(e))
            }

            Poll::Pending => {
                if let Some(sleep) = this.sleep.as_mut() {
                    if sleep.poll_unpin(cx).is_ready() {
                        tracing::info!(timeout = ?this.timeout, "Stream idle awaiting trailers");
                        this.metrics.counter.inc();

                        *this.sleep = None;
                        *this.idled_out = true;
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

#[cfg(test)]
mod tests {
    pub use super::*;
    use bytes::Bytes;
    use http_body::Body;
    use linkerd_stack::ServiceExt;

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn idle_before_response() {
        let _trace = linkerd_tracing::test::with_default_filter("trace");

        let metrics = Metrics::default();
        let (svc, mut handle) = tower_test::mock::pair::<_, http::Response<hyper::Body>>();
        let mut svc = StreamIdleTimeout::new(time::Duration::from_secs(100), metrics.clone(), svc);

        handle.allow(1);
        let call = svc.ready().await.unwrap().call(());
        let ((), _respond) = handle.next_request().await.unwrap();

        assert_eq!(metrics.counter.get(), 0);
        time::sleep(time::Duration::from_secs(100)).await;

        let rsp = call
            .now_or_never()
            .expect("call must be ready")
            .expect("call must not fail");
        assert_eq!(rsp.status().as_u16(), 408);
        assert_eq!(metrics.counter.get(), 1);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn idle_before_data() {
        let _trace = linkerd_tracing::test::with_default_filter("trace");

        let metrics = Metrics::default();
        let (svc, mut handle) = tower_test::mock::pair::<_, http::Response<hyper::Body>>();
        let mut svc = StreamIdleTimeout::new(time::Duration::from_secs(100), metrics.clone(), svc);

        handle.allow(1);
        let call = svc.ready().await.unwrap().call(());
        let ((), respond) = handle.next_request().await.unwrap();

        time::sleep(time::Duration::from_secs(90)).await;

        let (body_tx, body_rx) = hyper::Body::channel();
        respond.send_response(
            http::Response::builder()
                .status(http::StatusCode::OK)
                .body(body_rx)
                .unwrap(),
        );

        let rsp = call.await.expect("Call must not fail");
        assert_eq!(rsp.status().as_u16(), 200);
        assert_eq!(metrics.counter.get(), 0);

        time::sleep(time::Duration::from_secs(100)).await;
        assert!(rsp
            .into_body()
            .data()
            .now_or_never()
            .expect("EOS")
            .is_none());
        assert_eq!(metrics.counter.get(), 1);

        drop(body_tx);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn idle_resets_after_data() {
        let _trace = linkerd_tracing::test::with_default_filter("trace");

        let metrics = Metrics::default();
        let (svc, mut handle) = tower_test::mock::pair::<_, http::Response<hyper::Body>>();
        let mut svc = StreamIdleTimeout::new(time::Duration::from_secs(100), metrics.clone(), svc);

        handle.allow(1);
        let call = svc.ready().await.unwrap().call(());
        let ((), respond) = handle.next_request().await.unwrap();

        time::sleep(time::Duration::from_secs(90)).await;

        let (mut body_tx, body_rx) = hyper::Body::channel();
        respond.send_response(
            http::Response::builder()
                .status(http::StatusCode::OK)
                .body(body_rx)
                .unwrap(),
        );

        let mut rsp = call.await.expect("Call must not fail");
        assert_eq!(rsp.status().as_u16(), 200);

        body_tx
            .send_data(Bytes::from_static(b"hello"))
            .await
            .unwrap();
        assert_eq!(
            rsp.body_mut().data().await.unwrap().unwrap().as_ref(),
            b"hello"
        );

        time::sleep(time::Duration::from_secs(90)).await;
        assert!(rsp.body_mut().data().now_or_never().is_none());
        assert_eq!(metrics.counter.get(), 0);

        time::sleep(time::Duration::from_secs(10)).await;
        assert!(rsp.body_mut().data().now_or_never().expect("EOS").is_none());
        assert_eq!(metrics.counter.get(), 1);

        drop(body_tx);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn idle_trailers() {
        let _trace = linkerd_tracing::test::with_default_filter("trace");

        let metrics = Metrics::default();
        let (svc, mut handle) = tower_test::mock::pair::<_, http::Response<hyper::Body>>();
        let mut svc = StreamIdleTimeout::new(time::Duration::from_secs(100), metrics.clone(), svc);

        handle.allow(1);
        let call = svc.ready().await.unwrap().call(());
        let ((), respond) = handle.next_request().await.unwrap();

        time::sleep(time::Duration::from_secs(90)).await;

        let (mut body_tx, body_rx) = hyper::Body::channel();
        respond.send_response(
            http::Response::builder()
                .status(http::StatusCode::OK)
                .body(body_rx)
                .unwrap(),
        );

        let mut rsp = call.await.expect("Call must not fail");
        assert_eq!(rsp.status().as_u16(), 200);

        body_tx
            .send_data(Bytes::from_static(b"hello"))
            .await
            .unwrap();
        assert_eq!(
            rsp.body_mut().data().await.unwrap().unwrap().as_ref(),
            b"hello"
        );

        time::sleep(time::Duration::from_secs(90)).await;
        assert!(rsp.body_mut().trailers().now_or_never().is_none());
        assert_eq!(metrics.counter.get(), 0);

        time::sleep(time::Duration::from_secs(10)).await;
        assert!(rsp
            .body_mut()
            .trailers()
            .now_or_never()
            .expect("EOS")
            .expect("EOS")
            .is_none());
        assert_eq!(metrics.counter.get(), 1);

        drop(body_tx);
    }
}
