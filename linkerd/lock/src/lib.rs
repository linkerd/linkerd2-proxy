//! A middleware for sharing an inner service via mutual exclusion.

#![deny(warnings, rust_2018_idioms)]

pub mod error;
mod layer;
mod lock;
mod service;
mod shared;
#[cfg(test)]
mod test;

pub use self::{
    layer::LockLayer,
    lock::{Guard, Lock},
    service::LockService,
};
