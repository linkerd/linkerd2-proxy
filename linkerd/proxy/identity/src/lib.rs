#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]
#![allow(clippy::inconsistent_struct_constructor)]

pub mod certify;
pub mod metrics;

pub use self::certify::{AwaitCrt, CrtKeySender, LocalCrtKey};
