//! A middleware for sharing an inner service via mutual exclusion.

#![deny(warnings, rust_2018_idioms)]

pub mod error;
mod layer;
mod service;
mod shared;
#[cfg(test)]
mod test;

pub use self::{layer::LockLayer, service::Lock};
