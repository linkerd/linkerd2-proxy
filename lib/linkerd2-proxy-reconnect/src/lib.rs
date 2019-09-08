//! Conditionally reconnects with a pluggable recovery/backoff strategy.
#![deny(warnings, rust_2018_idioms)]

use linkerd2_proxy_core::Recover;

mod layer;
mod service;

pub use self::layer::Layer;
pub use self::service::Service;

pub fn layer<R: Recover + Clone>(recover: R) -> Layer<R> {
    recover.into()
}
