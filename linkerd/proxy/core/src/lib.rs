#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

pub mod resolve;

pub use self::resolve::{Resolve, ResolveService, Update};

/// A collection of services updated from a resolution.
pub trait Pool<T> {
    fn update_pool(&mut self, update: Update<T>);
}
