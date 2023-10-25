//! Adapted from [`tower::buffer`][buffer].
//!
//! [buffer]: https://github.com/tower-rs/tower/tree/bf4ea948346c59a5be03563425a7d9f04aadedf2/tower/src/buffer

#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

mod error;
mod failfast;
mod future;
mod message;
mod service;
mod worker;

pub use self::service::PoolQ;
pub use linkerd_proxy_core::Update;

use linkerd_stack::Service;

/// A collection of services updated from a resolution.
pub trait Pool<T, Req>: Service<Req> {
    fn update_pool(&mut self, update: Update<T>);

    fn poll_pool(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>>;
}
