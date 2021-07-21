#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]
#![allow(clippy::inconsistent_struct_constructor)]

pub mod recover;

pub use self::recover::Recover;
pub use std::convert::Infallible;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
