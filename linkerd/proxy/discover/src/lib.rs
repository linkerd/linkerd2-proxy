#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

pub mod buffer;
pub mod from_resolve;
pub mod make_endpoint;

pub use self::{buffer::Buffer, from_resolve::FromResolve, make_endpoint::MakeEndpoint};
