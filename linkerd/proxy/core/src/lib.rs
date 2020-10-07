#![deny(warnings, rust_2018_idioms)]

pub mod resolve;

pub use self::resolve::{Resolve, ResolveService, Update};
