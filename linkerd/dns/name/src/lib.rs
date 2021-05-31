#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]
#![allow(clippy::inconsistent_struct_constructor)]

mod name;
mod suffix;

pub use self::name::{InvalidName, Name};
pub use self::suffix::Suffix;
