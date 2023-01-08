use linkerd_app_core::{
    proxy::discover::{self, Buffer},
    svc::{layer, NewService},
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
    type Service = Buffer<discover::Stack<N, R, R::Endpoint>>;

    fn new_service(&self, target: T) -> Self::Service {
        let disco = discover::resolve(self.inner.clone(), self.resolve.clone());
        Buffer::new(ENDPOINT_BUFFER_CAPACITY, disco)
    }
}
