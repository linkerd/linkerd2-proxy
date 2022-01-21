#![deny(warnings, rust_2018_idioms, clippy::disallowed_method)]
#![forbid(unsafe_code)]

mod name;
mod suffix;

pub use self::name::{InvalidName, Name, NameRef};
pub use self::suffix::Suffix;
