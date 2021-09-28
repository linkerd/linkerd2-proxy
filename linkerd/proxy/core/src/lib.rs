#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]

pub mod resolve;

pub use self::resolve::{Resolve, ResolveService, Update};
