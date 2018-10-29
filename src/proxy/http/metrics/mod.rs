use indexmap::IndexMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_timer::clock;

use metrics::{latency, Counter, FmtLabels, Histogram};

mod class;
mod report;
mod service;
pub mod timestamp_request_open;

pub use self::report::Report;
pub use self::service::layer;

pub fn new<T, C>(retain_idle: Duration) -> (Arc<Mutex<Registry<T, C>>>, Report<T, C>)
where
    T: FmtLabels + Clone + Hash + Eq,
    C: FmtLabels + Hash + Eq,
{
    let registry = Arc::new(Mutex::new(Registry::default()));
    (registry.clone(), Report::new(retain_idle, registry))
}

#[derive(Debug)]
pub struct Registry<T, C>
where
    T: Hash + Eq,
    C: Hash + Eq,
{
    by_target: IndexMap<T, Arc<Mutex<Metrics<C>>>>,
}

#[derive(Debug)]
struct Metrics<C>
where
    C: Hash + Eq,
{
    last_update: Instant,
    total: Counter,
    by_class: IndexMap<C, ClassMetrics>,
    unclassified: ClassMetrics,
}

#[derive(Debug, Default)]
pub struct ClassMetrics {
    total: Counter,
    latency: Histogram<latency::Ms>,
}

impl<T, C> Default for Registry<T, C>
where
    T: Hash + Eq,
    C: Hash + Eq,
{
    fn default() -> Self {
        Self {
            by_target: IndexMap::default(),
        }
    }
}

impl<T, C> Registry<T, C>
where
    T: Hash + Eq,
    C: Hash + Eq,
{
    fn retain_since(&mut self, epoch: Instant) {
        self.by_target.retain(|_, m| m.lock().map(|m| m.last_update >= epoch).unwrap_or(false))
    }
}

impl<C> Default for Metrics<C>
where
    C: Hash + Eq,
{
    fn default() -> Self {
        Self {
            last_update: clock::now(),
            total: Counter::default(),
            by_class: IndexMap::default(),
            unclassified: ClassMetrics::default(),
        }
    }
}
