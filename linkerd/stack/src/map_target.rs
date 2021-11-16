use std::task::{Context, Poll};

pub trait MapTarget<T> {
    type Target;

    fn map_target(&self, t: T) -> Self::Target;
}

#[derive(Clone, Debug)]
pub struct MapTargetLayer<M>(M);

#[derive(Clone, Debug)]
pub struct MapTargetService<S, M> {
    inner: S,
    map_target: M,
}

impl<M> MapTargetLayer<M> {
    pub fn new(map_target: M) -> Self {
        MapTargetLayer(map_target)
    }
}

impl<S, M: Clone> tower::layer::Layer<S> for MapTargetLayer<M> {
    type Service = MapTargetService<S, M>;

    fn layer(&self, inner: S) -> Self::Service {
        Self::Service {
            inner,
            map_target: self.0.clone(),
        }
    }
}

impl<T, S, M> super::NewService<T> for MapTargetService<S, M>
where
    S: super::NewService<M::Target>,
    M: MapTarget<T>,
{
    type Service = S::Service;

    fn new_service(&self, target: T) -> Self::Service {
        self.inner.new_service(self.map_target.map_target(target))
    }
}

impl<T, S, M> tower::Service<T> for MapTargetService<S, M>
where
    S: tower::Service<M::Target>,
    M: MapTarget<T>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, target: T) -> Self::Future {
        self.inner.call(self.map_target.map_target(target))
    }
}

impl<F, T, U> MapTarget<T> for F
where
    F: Fn(T) -> U,
{
    type Target = U;
    fn map_target(&self, t: T) -> U {
        (self)(t)
    }
}
