use super::{Metrics, Sensor, SensorIo};
use linkerd_stack::{layer, ExtractParam, NewService, Service};
use std::{
    sync::Arc,
    task::{Context, Poll},
};

#[derive(Clone, Debug)]
pub struct NewServer<P, N> {
    params: P,
    inner: N,
}

#[derive(Clone, Debug)]
pub struct Server<S> {
    inner: S,
    metrics: Arc<Metrics>,
}

// === impl NewServer ===

impl<P: Clone, N> NewServer<P, N> {
    pub fn layer(params: P) -> impl layer::Layer<N, Service = Self> {
        layer::mk(move |inner| NewServer {
            params: params.clone(),
            inner,
        })
    }
}

impl<T, P, N> NewService<T> for NewServer<P, N>
where
    P: ExtractParam<Arc<Metrics>, T>,
    N: NewService<T>,
{
    type Service = Server<N::Service>;

    fn new_service(&mut self, target: T) -> Self::Service {
        let metrics = self.params.extract_param(&target);
        let inner = self.inner.new_service(target);
        Server { inner, metrics }
    }
}

// === impl Server ===

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
