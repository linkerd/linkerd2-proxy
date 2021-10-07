use super::{Metrics, Sensor, SensorIo};
use futures::{ready, TryFuture};
use linkerd_stack::{layer, ExtractParam, Service};
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
pub struct Connect<F> {
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
    S: Service<T>,
{
    type Response = SensorIo<S::Response>;
    type Error = S::Error;
    type Future = Connect<S::Future>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, target: T) -> Self::Future {
        let metrics = self.params.extract_param(&target);
        let inner = self.inner.call(target);
        Connect {
            metrics: Some(metrics),
            inner,
        }
    }
}

// === impl Connect ===

impl<F: TryFuture> Future for Connect<F> {
    type Output = Result<SensorIo<F::Ok>, F::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let io = ready!(this.inner.try_poll(cx))?;
        debug!("client connection open");

        let metrics = this
            .metrics
            .take()
            .expect("future must not be polled after ready");
        let t = SensorIo::new(io, Sensor::open(metrics));
        Poll::Ready(Ok(t))
    }
}
