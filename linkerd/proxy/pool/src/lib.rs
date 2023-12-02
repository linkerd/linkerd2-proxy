//! Adapted from [`tower::buffer`][buffer].
//!
//! [buffer]: https://github.com/tower-rs/tower/tree/bf4ea948346c59a5be03563425a7d9f04aadedf2/tower/src/buffer
//
// Copyright (c) 2019 Tower Contributors

#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

mod error;
mod failfast;
mod future;
mod message;
mod service;
#[cfg(test)]
mod tests;
mod worker;

pub use self::service::PoolQueue;
pub use linkerd_proxy_core::Update;

use linkerd_stack::Service;

/// A collection of services updated from a resolution.
pub trait Pool<T, Req>: Service<Req> {
    /// Updates the pool's endpoints.
    fn update_pool(&mut self, update: Update<T>);

    /// Polls to update the pool while the Service is ready.
    ///
    /// [`Service::poll_ready`] should do the same work, but will return ready
    /// as soon as there at least one ready endpoint. This method will continue
    /// to drive the pool until ready is returned (indicating that the pool need
    /// not be updated before another request is processed).
    fn poll_pool(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>>;
}
