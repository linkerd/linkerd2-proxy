//! A middleware that applies a layer to the inner `Service`'s (or
//! `NewService`'s) response.

use futures::{try_ready, Future, Poll};

/// Layers over services such that an `L`-typed Layer is applied to the result
/// of the inner Service or NewService.
#[derive(Clone, Debug)]
pub struct OnResponseLayer<L>(L);

/// Applies `L`-typed layers to the reresponses of an `S`-typed service.
#[derive(Clone, Debug)]
pub struct OnResponse<L, S> {
    inner: S,
    layer: L,
}

impl<L> OnResponseLayer<L> {
    pub fn new(layer: L) -> OnResponseLayer<L> {
        OnResponseLayer(layer)
    }
}

impl<M, L: Clone> tower::layer::Layer<M> for OnResponseLayer<L> {
    type Service = OnResponse<L, M>;

    fn layer(&self, inner: M) -> Self::Service {
        Self::Service {
            inner,
            layer: self.0.clone(),
        }
    }
}

impl<T, L, M> super::NewService<T> for OnResponse<L, M>
where
    L: tower::layer::Layer<M::Service>,
    M: super::NewService<T>,
{
    type Service = L::Service;

    fn new_service(&self, target: T) -> Self::Service {
        self.layer.layer(self.inner.new_service(target))
    }
}

impl<T, L, M> tower::Service<T> for OnResponse<L, M>
where
    L: tower::layer::Layer<M::Response> + Clone,
    M: tower::Service<T>,
{
    type Response = L::Service;
    type Error = M::Error;
    type Future = OnResponse<L, M::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        let inner = self.inner.call(target);
        Self::Future {
            layer: self.layer.clone(),
            inner,
        }
    }
}

impl<L, F> Future for OnResponse<L, F>
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
