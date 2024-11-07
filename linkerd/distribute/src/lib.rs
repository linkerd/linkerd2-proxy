//! A load-agnostic traffic distribution stack module.

#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

mod cache;
mod keys;
mod params;
mod service;
mod stack;

pub use self::{
    cache::{BackendCache, NewBackendCache},
    keys::WeightedServiceKeys,
    params::{Backends, Distribution},
    service::Distribute,
    stack::NewDistribute,
};
