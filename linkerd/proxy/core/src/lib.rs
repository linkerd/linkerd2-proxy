#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

pub mod meta;
pub mod resolve;

pub use self::{
    meta::Meta,
    resolve::{Resolve, ResolveService, Update},
};
