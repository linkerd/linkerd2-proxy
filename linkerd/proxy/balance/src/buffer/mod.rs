//! Adapted from [Tower's buffer module][buffer].
//!
//! [buffer]: https://github.com/tower-rs/tower/tree/bf4ea948346c59a5be03563425a7d9f04aadedf2/tower/src/buffer

mod error;
mod future;
mod message;
mod service;
mod worker;

pub use self::service::Buffer;

pub trait Pool<T> {
    fn update_pool(&mut self, update: linkerd_proxy_core::Update<T>);
}
