#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]
#![allow(clippy::inconsistent_struct_constructor)]

pub use self::{requests::Requests, retries::Retries};
use linkerd_metrics::{LastUpdate, Store};
use parking_lot::Mutex;
use std::fmt;
use std::hash::Hash;
use std::sync::Arc;
use std::time::Duration;

pub mod requests;
pub mod retries;

type Registry<T, M> = Store<T, Mutex<M>>;

/// Reports metrics for prometheus.
#[derive(Debug)]
pub struct Report<T, M>
where
    T: Hash + Eq,
{
    prefix: &'static str,
    registry: Arc<Mutex<Registry<T, M>>>,
    /// The amount of time metrics with no updates should be retained for reports
    retain_idle: Duration,
    /// Whether latencies should be reported.
    include_latencies: bool,
}

impl<T: Hash + Eq, M> Clone for Report<T, M> {
    fn clone(&self) -> Self {
        Self {
            include_latencies: self.include_latencies,
            prefix: self.prefix,
            registry: self.registry.clone(),
            retain_idle: self.retain_idle,
        }
    }
}

struct Prefixed<'p, N: fmt::Display> {
    prefix: &'p str,
    name: N,
}

impl<T, M> Report<T, M>
where
    T: Hash + Eq,
{
    fn new(retain_idle: Duration, registry: Arc<Mutex<Registry<T, M>>>) -> Self {
        Self {
            prefix: "",
            registry,
            retain_idle,
            include_latencies: true,
        }
    }

    pub fn with_prefix(self, prefix: &'static str) -> Self {
        if prefix.is_empty() {
            return self;
        }

        Self { prefix, ..self }
    }

    pub fn without_latencies(self) -> Self {
        Self {
            include_latencies: false,
            ..self
        }
    }

    fn prefix_key<N: fmt::Display>(&self, name: N) -> Prefixed<'_, N> {
        Prefixed {
            prefix: &self.prefix,
            name,
        }
    }
}

impl<'p, N: fmt::Display> fmt::Display for Prefixed<'p, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.prefix.is_empty() {
            return self.name.fmt(f);
        }

        write!(f, "{}_{}", self.prefix, self.name)
    }
}
