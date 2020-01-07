use futures::{try_ready, Future, Poll};

pub fn layer<L>(per_service: L) -> Layer<L> {
    Layer(per_service)
}

#[derive(Clone, Debug)]
pub struct Layer<L>(L);

/// Applies `L`-typed layers to the results of an `M`-typed service factory.
#[derive(Clone, Debug)]
pub struct PerService<L, M> {
    inner: M,
    layer: L,
}

pub struct MakeFuture<L, F> {
    inner: F,
    layer: L,
}

impl<M, L: Clone> tower::layer::Layer<M> for Layer<L> {
    type Service = PerService<L, M>;

    fn layer(&self, inner: M) -> Self::Service {
        Self::Service {
            inner,
            layer: self.0.clone(),
        }
    }
}

impl<T, L, M> super::NewService<T> for PerService<L, M>
where
    L: tower::layer::Layer<M::Service>,
    M: super::NewService<T>,
{
    type Service = L::Service;

    fn new_service(&self, target: T) -> Self::Service {
        self.layer.layer(self.inner.new_service(target))
    }
}

impl<T, L, M> tower::Service<T> for PerService<L, M>
where
    L: tower::layer::Layer<M::Response> + Clone,
    M: tower::Service<T>,
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
    L: tower::layer::Layer<F::Item>,
    F: Future,
{
    type Item = L::Service;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());
        Ok(self.layer.layer(inner).into())
    }
}
