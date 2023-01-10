use futures::prelude::*;
use linkerd_proxy_core::Resolve;
use linkerd_stack::NewService;
use std::{fmt::Debug, net::SocketAddr};

pub mod buffer;
pub mod from_resolve;
pub mod new;

pub use self::{from_resolve::FromResolve, new::DiscoverNew};

pub type Buffer<S> = buffer::Buffer<SocketAddr, S>;

pub(crate) fn spawn_new<T, R, M, N>(
    capacity: usize,
    resolve: R,
    new_service: M,
    target: T,
) -> Buffer<N::Service>
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
    let new_endpoint = new_service.new_service(target.clone());
    let resolution = resolve.resolve(target).try_flatten_stream();
    let disco = DiscoverNew::new(FromResolve::new(resolution), new_endpoint);
    buffer::spawn(capacity, disco)
}
