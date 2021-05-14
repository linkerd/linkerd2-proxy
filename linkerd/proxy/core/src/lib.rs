#![deny(warnings, rust_2018_idioms)]
#![allow(clippy::inconsistent_struct_constructor)]

pub mod resolve;

pub use self::resolve::{Resolve, ResolveService, Update};
