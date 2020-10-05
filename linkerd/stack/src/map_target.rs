use std::task::{Context, Poll};

pub trait MapTarget<T> {
    type Target;

    fn map_target(&self, t: T) -> Self::Target;
}

#[derive(Clone, Debug)]
pub struct MapTargetService<S, M> {
    inner: S,
    map_target: M,
}

impl<S, M> MapTargetService<S, M> {
    pub fn new(map_target: M, inner: S) -> Self {
        Self { inner, map_target }
    }
}

impl<T, S, M> super::NewService<T> for MapTargetService<S, M>
where
    S: super::NewService<M::Target>,
    M: MapTarget<T>,
{
    type Service = S::Service;

    fn new_service(&mut self, target: T) -> Self::Service {
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
