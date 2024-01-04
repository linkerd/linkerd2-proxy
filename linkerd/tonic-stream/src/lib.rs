use futures::FutureExt;
use linkerd_stack::{layer, ExtractParam, NewService, Service};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::time;

#[derive(Clone, Copy, Debug, Default)]
pub struct ReceiveLimits {
    /// Bounds the amount of time until a gRPC stream's initial item is
    /// received.
    pub initial: Option<time::Duration>,

    /// Bounds the amount of time between received gRPC stream items.
    pub idle: Option<time::Duration>,

    /// Bounds the total lifetime of a gRPC stream.
    pub lifetime: Option<time::Duration>,
}

#[derive(Clone, Debug)]
pub struct NewLimitReceive<P, N> {
    inner: N,
    params: P,
}

#[derive(Clone, Debug)]
pub struct LimitReceive<S> {
    inner: S,
    limits: ReceiveLimits,
}

#[pin_project::pin_project]
#[derive(Debug)]
pub struct LimitReceiveFuture<F> {
    #[pin]
    inner: F,
    limits: ReceiveLimits,
    recv: Option<Pin<Box<time::Sleep>>>,
}

#[pin_project::pin_project]
#[derive(Debug)]
pub struct LimitReceiveStream<S> {
    #[pin]
    inner: S,

    #[pin]
    lifetime: time::Sleep,

    recv: Pin<Box<time::Sleep>>,
    recv_timeout: Option<time::Duration>,
    recv_init: bool,
}

// === impl NewLimitReceive ===

impl<P: Clone, N> NewLimitReceive<P, N> {
    pub fn layer_via(params: P) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self {
            inner,
            params: params.clone(),
        })
    }
}

impl<T, P, N> NewService<T> for NewLimitReceive<P, N>
where
    N: NewService<T>,
    P: ExtractParam<ReceiveLimits, T>,
{
    type Service = LimitReceive<N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let limits = self.params.extract_param(&target);
        let inner = self.inner.new_service(target);
        LimitReceive { inner, limits }
    }
}

// === impl LimitReceive ===`

impl<S> LimitReceive<S> {
    pub fn new<Req, Rsp>(limits: ReceiveLimits, inner: S) -> Self
    where
        S: Service<tonic::Request<Req>, Response = tonic::Response<Rsp>, Error = tonic::Status>,
    {
        Self { inner, limits }
    }
}

impl<Req, Rsp, S> Service<tonic::Request<Req>> for LimitReceive<S>
where
    S: Service<tonic::Request<Req>, Response = tonic::Response<Rsp>, Error = tonic::Status>,
{
    type Response = tonic::Response<LimitReceiveStream<Rsp>>;
    type Error = tonic::Status;
    type Future = LimitReceiveFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: tonic::Request<Req>) -> Self::Future {
        let inner = self.inner.call(req);
        LimitReceiveFuture::new(self.limits, inner)
    }
}

// === impl LimitReceiveFuture ===

impl<S, F> LimitReceiveFuture<F>
where
    F: Future<Output = Result<tonic::Response<S>, tonic::Status>>,
{
    pub fn new(limits: ReceiveLimits, inner: F) -> Self {
        let recv = Some(Box::pin(time::sleep(
            limits
                .initial
                .or(limits.idle)
                .unwrap_or(time::Duration::MAX),
        )));
        Self {
            inner,
            limits,
            recv,
        }
    }
}

impl<S, F> Future for LimitReceiveFuture<F>
where
    F: Future<Output = Result<tonic::Response<S>, tonic::Status>>,
{
    type Output = Result<tonic::Response<LimitReceiveStream<S>>, tonic::Status>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        let rsp = if let Poll::Ready(res) = this.inner.poll(cx) {
            res?
        } else {
            futures::ready!(this.recv.as_mut().unwrap().poll_unpin(cx));
            return Poll::Ready(Err(tonic::Status::deadline_exceeded(
                "initial item not received within timeout",
            )));
        };

        let rsp = rsp.map(|inner| LimitReceiveStream {
            inner,
            recv: this.recv.take().unwrap(),
            recv_timeout: this.limits.idle,
            recv_init: true,
            lifetime: time::sleep(this.limits.lifetime.unwrap_or(time::Duration::MAX)),
        });
        Poll::Ready(Ok(rsp))
    }
}

// === impl ReceiveStream ===

impl<S> futures::Stream for LimitReceiveStream<S>
where
    S: futures::TryStream<Error = tonic::Status>,
{
    type Item = Result<S::Ok, tonic::Status>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();

        // If the lifetime has expired, end the stream.
        if this.lifetime.poll(cx).is_ready() {
            return Poll::Ready(None);
        }

        // Every time the inner stream yields an item, reset the receive
        // timeout.
        if let Poll::Ready(res) = this.inner.try_poll_next(cx) {
            *this.recv_init = false;
            if let Some(timeout) = this.recv_timeout {
                if let Some(t) = time::Instant::now().checked_add(*timeout) {
                    this.recv.as_mut().reset(t);
                } else {
                    tracing::error!("Receive timeout overflowed; ignoring")
                }
            }
            return Poll::Ready(res);
        }

        // If the receive timeout has expired, end the stream.
        if this.recv.poll_unpin(cx).is_ready() {
            return Poll::Ready(this.recv_init.then(|| {
                Err(tonic::Status::deadline_exceeded(
                    "Initial update not received within timeout",
                ))
            }));
        }

        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::prelude::*;
    use linkerd_stack::ServiceExt;
    use tokio_stream::wrappers::ReceiverStream;

    /// Tests that the initial timeout bounds the response from the inner
    /// service.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn init_timeout_response() {
        let _trace = linkerd_tracing::test::trace_init();

        let limits = ReceiveLimits {
            initial: Some(time::Duration::from_millis(1)),
            ..Default::default()
        };
        let svc = linkerd_stack::service_fn(|_: tonic::Request<()>| {
            futures::future::pending::<
                tonic::Result<tonic::Response<ReceiverStream<tonic::Result<()>>>>,
            >()
        });
        let svc = LimitReceive::new(limits, svc);

        let status = svc.oneshot(tonic::Request::new(())).await.unwrap_err();
        assert_eq!(status.code(), tonic::Code::DeadlineExceeded);
    }

    /// Tests that the initial timeout bounds the first item received from the
    /// inner response stream.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn init_timeout_recv() {
        let _trace = linkerd_tracing::test::trace_init();

        let limits = ReceiveLimits {
            initial: Some(time::Duration::from_millis(1)),
            ..Default::default()
        };
        let svc = linkerd_stack::service_fn(|_: tonic::Request<()>| {
            futures::future::ok::<_, tonic::Status>(tonic::Response::new(
                futures::stream::pending::<tonic::Result<()>>(),
            ))
        });
        let svc = LimitReceive::new(limits, svc);

        let rsp = svc
            .oneshot(tonic::Request::new(()))
            .await
            .unwrap()
            .into_inner();
        tokio::pin!(rsp);
        let status = rsp.try_next().await.unwrap_err();
        assert_eq!(status.code(), tonic::Code::DeadlineExceeded);
    }

    /// Tests that the receive timeout bounds idleness after the initial update.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn recv_timeout() {
        let _trace = linkerd_tracing::test::trace_init();

        let limits = ReceiveLimits {
            initial: Some(time::Duration::from_millis(1)),
            idle: Some(time::Duration::from_millis(1)),
            ..Default::default()
        };
        let (tx, rx) = tokio::sync::mpsc::channel::<tonic::Result<()>>(2);
        let svc = {
            let mut rx = Some(rx);
            linkerd_stack::service_fn(move |_: tonic::Request<()>| {
                futures::future::ok::<_, tonic::Status>(tonic::Response::new(ReceiverStream::new(
                    rx.take().unwrap(),
                )))
            })
        };
        let svc = LimitReceive::new(limits, svc);

        let rsp = svc
            .oneshot(tonic::Request::new(()))
            .await
            .unwrap()
            .into_inner();
        tokio::pin!(rsp);

        tx.send(Ok(())).await.unwrap();
        assert!(rsp.try_next().await.is_ok());

        let res = rsp.try_next().await.expect("stream should not error");
        assert_eq!(res, None);
    }

    /// Tests that the lifetime bounds the total duration of the stream.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn lifetime() {
        let _trace = linkerd_tracing::test::trace_init();

        let limits = ReceiveLimits {
            lifetime: Some(time::Duration::from_millis(10)),
            ..Default::default()
        };
        let (tx, rx) = tokio::sync::mpsc::channel::<tonic::Result<()>>(2);
        let svc = {
            let mut rx = Some(rx);
            linkerd_stack::service_fn(move |_: tonic::Request<()>| {
                futures::future::ok::<_, tonic::Status>(tonic::Response::new(ReceiverStream::new(
                    rx.take().unwrap(),
                )))
            })
        };
        let svc = LimitReceive::new(limits, svc);

        let rsp = svc
            .oneshot(tonic::Request::new(()))
            .await
            .unwrap()
            .into_inner();
        tokio::pin!(rsp);

        tx.send(Ok(())).await.unwrap();
        time::sleep(time::Duration::from_millis(5)).await;
        assert!(rsp.try_next().await.is_ok());

        tx.send(Ok(())).await.unwrap();
        time::sleep(time::Duration::from_millis(9)).await;
        assert!(rsp.try_next().await.is_ok());

        tx.send(Ok(())).await.unwrap();
        time::sleep(time::Duration::from_millis(10)).await;
        assert!(rsp.try_next().await.is_ok());
    }
}
