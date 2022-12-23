//! A load-agnostic traffic distribution stack module.

#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

mod params;
mod service;
mod stack;

pub use self::{
    params::{Distribution, WeightedKeys},
    service::Distribute,
    stack::NewDistribute,
};
