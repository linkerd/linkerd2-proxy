use futures::{Future, Poll};
use svc;

pub fn layer<L>(per_make: L) -> Layer<L> {
    Layer(per_make)
}

#[derive(Clone, Debug)]
pub struct Layer<L>(L);

#[derive(Clone, Debug)]
pub struct PerMake<L, M> {
    inner: M,
    layer: L,
}

pub struct MakeFuture<L, F> {
    inner: F,
    layer: L,
}

impl<M, L: Clone> super::Layer<M> for Layer<L> {
    type Service = PerMake<L, M>;

    fn layer(&self, inner: M) -> Self::Service {
        PerMake {
            inner,
            layer: self.0.clone(),
        }
    }
}

impl<T, L, M> svc::Service<T> for PerMake<L, M>
where
    L: super::Layer<M::Response> + Clone,
    M: svc::Service<T>,
{
    type Response = L::Service;
    type Error = M::Error;
    type Future = MakeFuture<L, M::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        let inner = self.inner.call(target);
        MakeFuture {
            layer: self.layer.clone(),
            inner,
        }
    }
}

impl<L, F> Future for MakeFuture<L, F>
where
    L: super::Layer<F::Item>,
    F: Future,
{
    type Item = L::Service;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());
        Ok(self.layer.layer(inner).into())
    }
}
