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

/// A collection of services updated from a resolution.
pub trait Pool<T> {
    fn update_pool(&mut self, update: Update<T>);
}
