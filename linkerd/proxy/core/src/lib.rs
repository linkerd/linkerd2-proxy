#![deny(warnings, rust_2018_idioms, clippy::disallowed_method)]
#![forbid(unsafe_code)]

pub mod resolve;

pub use self::resolve::{Resolve, ResolveService, Update};
