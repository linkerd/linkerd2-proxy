#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use futures::prelude::*;
use linkerd_proxy_core::Resolve;
use linkerd_stack::{layer, NewService};
use std::{fmt::Debug, net::SocketAddr};

pub mod buffer;
pub mod from_resolve;
pub mod new;

pub use self::{from_resolve::FromResolve, new::DiscoverNew};

pub type Result<S> = buffer::Result<SocketAddr, S>;
pub type Buffer<S> = buffer::Buffer<SocketAddr, S>;

#[derive(Clone, Debug)]
pub struct NewSpawnDiscover<R, N> {
    capacity: usize,
    resolve: R,
    inner: N,
}

impl<R, N> NewSpawnDiscover<R, N> {
    pub fn new(capacity: usize, resolve: R, inner: N) -> Self {
        Self {
            capacity,
            resolve,
            inner,
        }
    }

    pub fn layer(capacity: usize, resolve: R) -> impl layer::Layer<N, Service = Self> + Clone
    where
        R: Clone,
    {
        layer::mk(move |inner| Self::new(capacity, resolve.clone(), inner))
    }
}

impl<T, R, M, N> NewService<T> for NewSpawnDiscover<R, M>
where
    T: Clone,
    R: Resolve<T> + Clone,
    R::Endpoint: Clone + Debug + Eq + Send + 'static,
    R::Resolution: Send,
    R::Future: Send + 'static,
    M: NewService<T, Service = N>,
    N: NewService<(SocketAddr, R::Endpoint)> + Send + 'static,
    N::Service: Send,
{
    type Service = Buffer<N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let inner = self.inner.new_service(target.clone());
        let resoln = self.resolve.resolve(target).try_flatten_stream();
        let disco = DiscoverNew::new(FromResolve::new(resoln), inner);
        buffer::spawn(self.capacity, disco)
    }
}
