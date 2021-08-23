use super::{Metrics, Sensor, SensorIo};
use linkerd_metrics::NewMetrics;
use linkerd_stack::Service;
use std::{
    sync::Arc,
    task::{Context, Poll},
};

pub type NewServer<N, K, S> = NewMetrics<N, K, Metrics, Server<S>>;

#[derive(Clone, Debug)]
pub struct Server<S> {
    inner: S,
    metrics: Arc<Metrics>,
}

// === impl Accept ===

impl<A> From<(A, Arc<Metrics>)> for Server<A> {
    fn from((inner, metrics): (A, Arc<Metrics>)) -> Self {
        Self { inner, metrics }
    }
}

impl<I, A> Service<I> for Server<A>
where
    A: Service<SensorIo<I>, Response = ()>,
{
    type Response = ();
    type Error = A::Error;
    type Future = A::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, io: I) -> Self::Future {
        let io = SensorIo::new(io, Sensor::open(self.metrics.clone()));
        self.inner.call(io)
    }
}
