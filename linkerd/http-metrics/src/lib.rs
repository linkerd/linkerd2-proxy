#![deny(warnings, rust_2018_idioms)]

pub use self::{requests::Requests, retries::Retries};
use indexmap::IndexMap;
use std::fmt;
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub mod requests;
pub mod retries;

#[derive(Debug)]
struct Registry<T, M>
where
    T: Hash + Eq,
{
    by_target: IndexMap<T, Arc<Mutex<M>>>,
}

/// Reports metrics for prometheus.
#[derive(Debug)]
pub struct Report<T, M>
where
    T: Hash + Eq,
{
    prefix: &'static str,
    registry: Arc<Mutex<Registry<T, M>>>,
    retain_idle: Duration,
}

impl<T: Hash + Eq, M> Clone for Report<T, M> {
    fn clone(&self) -> Self {
        Self {
            prefix: self.prefix.clone(),
            registry: self.registry.clone(),
            retain_idle: self.retain_idle,
        }
    }
}

struct Prefixed<'p, N: fmt::Display> {
    prefix: &'p str,
    name: N,
}

trait LastUpdate {
    fn last_update(&self) -> Instant;
}

impl<T, M> Default for Registry<T, M>
where
    T: Hash + Eq,
{
    fn default() -> Self {
        Self {
            by_target: IndexMap::default(),
        }
    }
}

impl<T, M> Registry<T, M>
where
    T: Hash + Eq,
    M: LastUpdate,
{
    /// Retains metrics for all targets that (1) no longer have an active
    /// reference to the `RequestMetrics` structure and (2) have not been updated since `epoch`.
    fn retain_since(&mut self, epoch: Instant) {
        self.by_target.retain(|_, m| {
            Arc::strong_count(&m) > 1 || m.lock().map(|m| m.last_update() >= epoch).unwrap_or(false)
        })
    }
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
        }
    }

    pub fn with_prefix(self, prefix: &'static str) -> Self {
        if prefix.is_empty() {
            return self;
        }

        Self { prefix, ..self }
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
