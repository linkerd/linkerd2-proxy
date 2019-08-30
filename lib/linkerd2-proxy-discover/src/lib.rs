#![deny(warnings, rust_2018_idioms)]

mod discover;
mod service;

pub use self::discover::Discover;
pub use self::service::{DiscoverFuture, MakeDiscover};
