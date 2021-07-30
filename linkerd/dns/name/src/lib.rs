#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]

mod name;
mod suffix;

pub use self::name::{InvalidName, Name};
pub use self::suffix::Suffix;
