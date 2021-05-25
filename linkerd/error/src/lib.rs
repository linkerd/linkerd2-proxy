#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]
#![allow(clippy::inconsistent_struct_constructor)]

mod never;
pub mod recover;

pub use self::never::Never;
pub use self::recover::Recover;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
