//! A middleware that applies a layer to the inner `Service`'s (or
//! `NewService`'s) response.
use futures::{ready, TryFuture};
use pin_project::pin_project;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

/// Layers over services such that an `L`-typed Layer is applied to the result
/// of the inner Service or NewService.
#[derive(Clone, Debug)]
pub struct OnServiceLayer<L>(L);

/// Applies `L`-typed layers to the responses of an `S`-typed service.
#[pin_project]
#[derive(Clone, Debug)]
pub struct OnService<L, S> {
    #[pin]
    inner: S,
    layer: L,
}

impl<L> OnServiceLayer<L> {
    pub fn new(layer: L) -> OnServiceLayer<L> {
        OnServiceLayer(layer)
    }
}

impl<M, L: Clone> tower::layer::Layer<M> for OnServiceLayer<L> {
    type Service = OnService<L, M>;

    fn layer(&self, inner: M) -> Self::Service {
        Self::Service {
            inner,
            layer: self.0.clone(),
        }
    }
}

impl<T, L, M> super::NewService<T> for OnService<L, M>
where
    L: tower::layer::Layer<M::Service>,
    M: super::NewService<T>,
{
    type Service = L::Service;

    fn new_service(&self, target: T) -> Self::Service {
        self.layer.layer(self.inner.new_service(target))
    }
}

impl<T, L, M> tower::Service<T> for OnService<L, M>
where
    L: tower::layer::Layer<M::Response> + Clone,
    M: tower::Service<T>,
{
    type Response = L::Service;
    type Error = M::Error;
    type Future = OnService<L, M::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, target: T) -> Self::Future {
        let inner = self.inner.call(target);
        Self::Future {
            layer: self.layer.clone(),
            inner,
        }
    }
}

impl<L, F> Future for OnService<L, F>
where
    L: tower::layer::Layer<F::Ok>,
    F: TryFuture,
{
    type Output = Result<L::Service, F::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let inner = ready!(this.inner.try_poll(cx))?;
        Poll::Ready(Ok(this.layer.layer(inner)))
    }
}
