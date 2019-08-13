#![deny(warnings, rust_2018_idioms)]

mod name;
mod suffix;

pub use self::name::{InvalidName, Name};
pub use self::suffix::Suffix;
