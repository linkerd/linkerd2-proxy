use linkerd_app_core::{
    proxy::discover::{buffer, Buffer, FromResolve, MakeEndpoint},
    svc::{layer, FutureService, NewService},
};
use std::time::Duration;

pub use linkerd_app_core::{
    core::Resolve,
    proxy::api_resolve::{ConcreteAddr, Metadata},
    resolve::map_endpoint,
};

const ENDPOINT_BUFFER_CAPACITY: usize = 1_000;

#[derive(Clone, Debug)]
pub struct NewResolve<R, N> {
    resolve: R,
    inner: N,
}

impl<R, N> NewResolve<R, N> {
    pub fn new(resolve: R, inner: N) -> Self {
        Self { resolve, inner }
    }

    pub fn layer(resolve: R) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(resolve.clone(), inner))
    }
}

impl<T, R, N: NewService<T>> NewService<T> for NewResolve<R, N>
where
    T: Clone + Send + std::fmt::Debug,
    R: Resolve<T> + Clone,
    R::Resolution: Send,
    R::Future: Send,
    N: NewService<R::Endpoint>,
{
    type Service = discover::Buffer<R::Endpoint, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let resolve = self.resolve.clone().resolve(target);

        buffer::spawn(
            ENDPOINT_BUFFER_CAPACITY,
            MakeEndpoint::new(
                self.inner.clone(),
                FromResolve::new(FutureService::new(resolve)),
            ),
        )
    }
}
