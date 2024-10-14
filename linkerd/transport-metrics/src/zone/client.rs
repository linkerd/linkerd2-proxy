use crate::zone::{
    sensor::{ZoneMetricsSensor, ZoneSensorIo},
    TcpZoneMetrics,
};
use futures::{ready, TryFuture};
use linkerd_stack::{layer, ExtractParam, MakeConnection, Service};
use pin_project::pin_project;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

#[derive(Clone, Debug)]
pub struct ZoneMetricsClient<P, S> {
    inner: S,
    params: P,
}

#[pin_project]
pub struct ConnectFuture<F> {
    #[pin]
    inner: F,
    metrics: Option<TcpZoneMetrics>,
}

// === impl Client ===

impl<P: Clone, S> ZoneMetricsClient<P, S> {
    pub fn layer(params: P) -> impl layer::Layer<S, Service = Self> + Clone {
        layer::mk(move |inner| Self {
            inner,
            params: params.clone(),
        })
    }
}

impl<T, P, S> Service<T> for ZoneMetricsClient<P, S>
where
    P: ExtractParam<TcpZoneMetrics, T>,
    S: MakeConnection<T>,
{
    type Response = (ZoneSensorIo<S::Connection>, S::Metadata);
    type Error = S::Error;
    type Future = ConnectFuture<S::Future>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, target: T) -> Self::Future {
        let metrics = self.params.extract_param(&target);
        let inner = self.inner.connect(target);
        ConnectFuture {
            metrics: Some(metrics),
            inner,
        }
    }
}

// === impl ConnectFuture ===

impl<I, M, F: TryFuture<Ok = (I, M)>> Future for ConnectFuture<F> {
    type Output = Result<(ZoneSensorIo<I>, M), F::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let (io, meta) = ready!(this.inner.try_poll(cx))?;
        let metrics = this
            .metrics
            .take()
            .expect("future must not be polled after ready");
        let io = ZoneSensorIo::new(io, ZoneMetricsSensor { metrics });
        Poll::Ready(Ok((io, meta)))
    }
}
