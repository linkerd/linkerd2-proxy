use super::{Metrics, Sensor, SensorIo};
use futures::{ready, TryFuture};
use linkerd_stack::{layer, ExtractParam, MakeConnection, Service};
use pin_project::pin_project;
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tracing::debug;

#[derive(Clone, Debug)]
pub struct Client<P, S> {
    inner: S,
    params: P,
}

#[pin_project]
pub struct ConnectFuture<F> {
    #[pin]
    inner: F,
    metrics: Option<Arc<Metrics>>,
}

// === impl Client ===

impl<P: Clone, S> Client<P, S> {
    pub fn layer(params: P) -> impl layer::Layer<S, Service = Self> + Clone {
        layer::mk(move |inner| Self {
            inner,
            params: params.clone(),
        })
    }
}

impl<T, P, S> Service<T> for Client<P, S>
where
    P: ExtractParam<Arc<Metrics>, T>,
    S: MakeConnection<T>,
{
    type Response = (SensorIo<S::Connection>, S::Metadata);
    type Error = S::Error;
    type Future = ConnectFuture<S::Future>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, target: T) -> Self::Future {
        let metrics = self.params.extract_param(&target);
        let inner = self.inner.make_connection(target);
        ConnectFuture {
            metrics: Some(metrics),
            inner,
        }
    }
}

// === impl ConnectFuture ===

impl<I, M, F: TryFuture<Ok = (I, M)>> Future for ConnectFuture<F> {
    type Output = Result<(SensorIo<I>, M), F::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let (io, meta) = ready!(this.inner.try_poll(cx))?;
        debug!("client connection open");

        let metrics = this
            .metrics
            .take()
            .expect("future must not be polled after ready");
        let io = SensorIo::new(io, Sensor::open(metrics));
        Poll::Ready(Ok((io, meta)))
    }
}
