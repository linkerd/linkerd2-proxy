//! Adapted from [Tower's buffer module][buffer].
//!
//! [buffer]: https://github.com/tower-rs/tower/tree/bf4ea948346c59a5be03563425a7d9f04aadedf2/tower/src/buffer

mod error;
mod failfast;
mod future;
mod message;
mod service;
mod worker;

pub use self::service::Buffer;
