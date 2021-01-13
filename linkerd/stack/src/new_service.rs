use crate::FutureService;
use futures::future;
use linkerd_error::Never;
use std::task::{Context, Poll};
use tower::util::{Oneshot, ServiceExt};

/// Immediately and infalliby creates (usually) a Service.
pub trait NewService<T> {
    type Service;

    fn new_service(&mut self, target: T) -> Self::Service;
}

/// A Layer that modifies inner `MakeService`s to be exposd as a `NewService`.
#[derive(Clone, Copy, Debug, Default)]
pub struct FromMakeServiceLayer(());

/// Modifies inner `MakeService`s to be exposd as a `NewService`.
#[derive(Clone, Copy, Debug)]
pub struct FromMakeService<S> {
    make_service: S,
}

/// Modifies inner `MakeService`s to be exposd as a `NewService`.
#[derive(Clone, Copy, Debug)]
pub struct IntoMakeService<N> {
    new_service: N,
}

// === impl NewService ===

impl<F, T, S> NewService<T> for F
where
    F: Fn(T) -> S,
{
    type Service = S;

    fn new_service(&mut self, target: T) -> Self::Service {
        (self)(target)
    }
}

// === impl FromMakeServiceLayer ===

impl<S> tower::layer::Layer<S> for FromMakeServiceLayer {
    type Service = FromMakeService<S>;

    fn layer(&self, make_service: S) -> Self::Service {
        Self::Service { make_service }
    }
}

// === impl FromMakeService ===

impl<T, S> NewService<T> for FromMakeService<S>
where
    S: tower::Service<T> + Clone,
{
    type Service = FutureService<Oneshot<S, T>, S::Response>;

    fn new_service(&mut self, target: T) -> Self::Service {
        FutureService::new(self.make_service.clone().oneshot(target))
    }
}

// === impl FromMakeService ===

impl<N> IntoMakeService<N> {
    pub fn new(new_service: N) -> Self {
        Self { new_service }
    }
}

impl<T, N: NewService<T>> tower::Service<T> for IntoMakeService<N> {
    type Response = N::Service;
    type Error = Never;
    type Future = future::Ready<Result<N::Service, Never>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Never>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: T) -> Self::Future {
        future::ok(self.new_service.new_service(target))
    }
}
