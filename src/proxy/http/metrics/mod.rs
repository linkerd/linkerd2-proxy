pub mod classify;
mod report;
mod service;

pub use self::report::Report;
pub use self::service::layer;
use http;
use indexmap::IndexMap;
use linkerd2_metrics::{latency, Counter, FmtLabels, Histogram};
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_timer::clock;

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
    by_target: IndexMap<T, Arc<Mutex<RequestMetrics<C>>>>,
}

pub trait Scoped<T> {
    type Scope: Stats;
    fn scoped(&self, index: T) -> Self::Scope;
}

pub trait Stats {
    fn incr_retry_skipped_budget(&self);
}

#[derive(Debug)]
pub struct RequestMetrics<C>
where
    C: Hash + Eq,
{
    last_update: Instant,
    total: Counter,
    by_retry_skipped: IndexMap<RetrySkipped, Counter>,
    by_status: IndexMap<Option<http::StatusCode>, StatusMetrics<C>>,
}

#[derive(Debug)]
struct StatusMetrics<C>
where
    C: Hash + Eq,
{
    latency: Histogram<latency::Ms>,
    by_class: IndexMap<C, ClassMetrics>,
}

#[derive(Debug, Default)]
pub struct ClassMetrics {
    total: Counter,
}

#[derive(Debug, PartialEq, Eq, Hash)]
enum RetrySkipped {
    Budget,
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
    /// Retains metrics for all targets that (1) no longer have an active
    /// reference to the `RequestMetrics` structure and (2) have not been updated since `epoch`.
    fn retain_since(&mut self, epoch: Instant) {
        self.by_target.retain(|_, m| {
            Arc::strong_count(&m) > 1 || m.lock().map(|m| m.last_update >= epoch).unwrap_or(false)
        })
    }
}

impl<T, C> Scoped<T> for Arc<Mutex<Registry<T, C>>>
where
    T: Hash + Eq,
    C: Hash + Eq,
{
    type Scope = Arc<Mutex<RequestMetrics<C>>>;

    fn scoped(&self, target: T) -> Self::Scope {
        self.lock()
            .expect("metrics Registry lock")
            .by_target
            .entry(target)
            .or_insert_with(|| Arc::new(Mutex::new(RequestMetrics::default())))
            .clone()
    }
}

impl<C> RequestMetrics<C>
where
    C: Hash + Eq,
{
    fn incr_retry_skipped(&mut self, reason: RetrySkipped) {
        self.by_retry_skipped
            .entry(reason)
            .or_insert_with(Counter::default)
            .incr();
    }
}

impl<C> Default for RequestMetrics<C>
where
    C: Hash + Eq,
{
    fn default() -> Self {
        Self {
            last_update: clock::now(),
            total: Counter::default(),
            by_retry_skipped: IndexMap::default(),
            by_status: IndexMap::default(),
        }
    }
}

impl<C> Stats for Arc<Mutex<RequestMetrics<C>>>
where
    C: Hash + Eq,
{
    fn incr_retry_skipped_budget(&self) {
        if let Ok(mut metrics) = self.lock() {
            metrics.last_update = clock::now();
            metrics.incr_retry_skipped(RetrySkipped::Budget);
        }
    }
}

impl<C> Default for StatusMetrics<C>
where
    C: Hash + Eq,
{
    fn default() -> Self {
        Self {
            latency: Histogram::default(),
            by_class: IndexMap::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn expiry() {
        use linkerd2_metrics::FmtLabels;
        use std::fmt;
        use std::time::Duration;
        use tokio_timer::clock;

        #[derive(Clone, Debug, Hash, Eq, PartialEq)]
        struct Target(usize);
        impl FmtLabels for Target {
            fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "n=\"{}\"", self.0)
            }
        }

        #[allow(dead_code)]
        #[derive(Clone, Debug, Hash, Eq, PartialEq)]
        enum Class {
            Good,
            Bad,
        };
        impl FmtLabels for Class {
            fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                use std::fmt::Display;
                match self {
                    Class::Good => "class=\"good\"".fmt(f),
                    Class::Bad => "class=\"bad\"".fmt(f),
                }
            }
        }

        let retain_idle_for = Duration::from_secs(1);
        let (r, report) = super::new::<Target, Class>(retain_idle_for);
        let mut registry = r.lock().unwrap();

        let before_update = clock::now();
        let metrics = registry
            .by_target
            .entry(Target(123))
            .or_insert_with(|| Default::default())
            .clone();
        assert_eq!(registry.by_target.len(), 1, "target should be registered");
        let after_update = clock::now();

        registry.retain_since(after_update);
        assert_eq!(
            registry.by_target.len(),
            1,
            "target should not be evicted by time alone"
        );

        drop(metrics);
        registry.retain_since(before_update);
        assert_eq!(
            registry.by_target.len(),
            1,
            "target should not be evicted by availability alone"
        );

        registry.retain_since(after_update);
        assert_eq!(
            registry.by_target.len(),
            0,
            "target should be evicted by time once the handle is dropped"
        );

        drop((registry, report));
    }
}
