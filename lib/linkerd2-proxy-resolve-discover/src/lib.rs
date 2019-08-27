#![deny(warnings, rust_2018_idioms)]

mod discover;
mod layer;

pub use self::discover::Discover;
pub use self::layer::{layer, DiscoverFuture, Layer, MakeDiscover};
