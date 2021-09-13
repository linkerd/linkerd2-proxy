use crate::{layer, NewService, Service};
use futures::TryFuture;
use pin_project::pin_project;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

/// A strategy for monitoring a NewService.
pub trait MonitorNewService<T> {
    type MonitorService;

    fn monitor(&self, target: &T) -> Self::MonitorService;
}

/// A strategy for monitoring a Service.
pub trait MonitorService<Req> {
    type MonitorResponse;

    /// Monitors a response.
    fn monitor_request(&mut self, req: &Req) -> Self::MonitorResponse;
}

/// A strategy for monitoring a Service's errors
pub trait MonitorError<E> {
    fn monitor_error(&mut self, err: &E);
}

#[derive(Clone, Debug)]
pub struct NewMonitor<M, N> {
    monitor: M,
    inner: N,
}

#[derive(Clone, Debug)]
pub struct Monitor<M, S> {
    monitor: M,
    inner: S,
}

#[pin_project]
#[derive(Debug)]
pub struct MonitorFuture<M, F> {
    monitor: M,
    #[pin]
    inner: F,
}

// === impl NewMonitor ===

impl<M: Clone, N> NewMonitor<M, N> {
    pub fn layer(monitor: M) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self {
            inner,
            monitor: monitor.clone(),
        })
    }
}

impl<T, M, N> NewService<T> for NewMonitor<M, N>
where
    M: MonitorNewService<T>,
    N: NewService<T>,
{
    type Service = Monitor<M::MonitorService, N::Service>;

    #[inline]
    fn new_service(&self, target: T) -> Self::Service {
        let monitor = self.monitor.monitor(&target);
        let inner = self.inner.new_service(target);
        Monitor { monitor, inner }
    }
}

// === impl Monitor ===

impl<M: Clone, S> Monitor<M, S> {
    pub fn layer(monitor: M) -> impl layer::Layer<S, Service = Self> + Clone {
        layer::mk(move |inner| Self {
            inner,
            monitor: monitor.clone(),
        })
    }
}

impl<Req, M, S> Service<Req> for Monitor<M, S>
where
    S: Service<Req>,
    M: MonitorError<S::Error>,
    M: MonitorService<Req>,
    M::MonitorResponse: MonitorError<S::Error>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = MonitorFuture<M::MonitorResponse, S::Future>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match futures::ready!(self.inner.poll_ready(cx)) {
            Ok(()) => Poll::Ready(Ok(())),
            Err(err) => {
                self.monitor.monitor_error(&err);
                Poll::Ready(Err(err))
            }
        }
    }

    #[inline]
    fn call(&mut self, req: Req) -> Self::Future {
        let monitor = self.monitor.monitor_request(&req);
        let inner = self.inner.call(req);
        MonitorFuture { monitor, inner }
    }
}

// === impl MonitorFuture ===

impl<M, F> Future for MonitorFuture<M, F>
where
    M: MonitorError<F::Error>,
    F: TryFuture,
{
    type Output = Result<F::Ok, F::Error>;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        match futures::ready!(this.inner.try_poll(cx)) {
            Ok(rsp) => Poll::Ready(Ok(rsp)),
            Err(err) => {
                this.monitor.monitor_error(&err);
                Poll::Ready(Err(err))
            }
        }
    }
}
