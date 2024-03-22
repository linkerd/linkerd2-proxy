#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use std::{
    net::SocketAddr,
    task::{Context, Poll},
};
use tower_service::Service;

/// A collection of services updated from a resolution.
pub trait Pool<T, Req>: Service<Req> {
    fn reset_pool(&mut self, update: Vec<(SocketAddr, T)>);

    fn add_endpoint(&mut self, addr: SocketAddr, endpoint: T);

    fn remove_endpoint(&mut self, addr: SocketAddr);

    /// Polls to update the pool while the Service is ready.
    ///
    /// [`Service::poll_ready`] should do the same work, but will return ready
    /// as soon as there at least one ready endpoint. This method will continue
    /// to drive the pool until ready is returned (indicating that the pool need
    /// not be updated before another request is processed).
    fn poll_pool(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>>;
}
