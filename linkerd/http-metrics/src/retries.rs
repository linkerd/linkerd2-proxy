use super::{LastUpdate, Prefixed, Registry, Report};
use linkerd_metrics::{Counter, FmtLabels, FmtMetric, FmtMetrics, Metric};
use parking_lot::Mutex;
use std::{
    fmt,
    hash::Hash,
    sync::Arc,
    time::{Duration, Instant},
};
use tracing::trace;

#[derive(Debug)]
pub struct Retries<T>(Arc<Mutex<Registry<T, Metrics>>>)
where
    T: Hash + Eq;

#[derive(Clone, Debug)]
pub struct Handle(Arc<Mutex<Metrics>>);

#[derive(Debug)]
pub struct Metrics {
    last_update: Instant,
    retryable: Counter,
    no_budget: Counter,
}

struct NoBudgetLabel;

// === impl Retries ===

impl<T: Hash + Eq> Default for Retries<T> {
    fn default() -> Self {
        Retries(Arc::new(Mutex::new(Registry::default())))
    }
}

impl<T: Hash + Eq> Retries<T> {
    pub fn into_report(self, retain_idle: Duration) -> Report<T, Metrics> {
        Report::new(retain_idle, self.0)
    }

    pub fn get_handle(&self, target: T) -> Handle {
        let mut reg = self.0.lock();
        Handle(reg.entry(target).or_default().clone())
    }
}

impl<T: Hash + Eq> Clone for Retries<T> {
    fn clone(&self) -> Self {
        Retries(self.0.clone())
    }
}

// === impl Handle ===

impl Handle {
    pub fn incr_retryable(&self, has_budget: bool) {
        let mut m = self.0.lock();
        m.last_update = Instant::now();
        m.retryable.incr();
        if !has_budget {
            m.no_budget.incr();
        }
    }
}

// === impl Metrics ===

impl Default for Metrics {
    fn default() -> Self {
        Self {
            last_update: Instant::now(),
            retryable: Counter::default(),
            no_budget: Counter::default(),
        }
    }
}

impl LastUpdate for Metrics {
    fn last_update(&self) -> Instant {
        self.last_update
    }
}

// === impl Report ===

impl<T> Report<T, Metrics>
where
    T: FmtLabels + Hash + Eq,
{
    fn retryable_total(&self) -> Metric<'_, Prefixed<'_, &'static str>, Counter> {
        Metric::new(
            self.prefix_key("retryable_total"),
            "Total count of retryable HTTP responses.",
        )
    }
}

impl<T> FmtMetrics for Report<T, Metrics>
where
    T: FmtLabels + Hash + Eq,
{
    fn fmt_metrics(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut registry = self.registry.lock();
        trace!(
            prfefix = %self.prefix,
            targets = %registry.len(),
            "Formatting HTTP retry metrics",
        );

        if registry.is_empty() {
            return Ok(());
        }

        let metric = self.retryable_total();
        metric.fmt_help(f)?;
        for (tgt, tm) in registry.iter() {
            let m = tm.lock();
            m.retryable.fmt_metric_labeled(f, &metric.name, tgt)?;
            m.no_budget
                .fmt_metric_labeled(f, &metric.name, (tgt, NoBudgetLabel))?;
        }

        registry.retain_since(Instant::now() - self.retain_idle);

        Ok(())
    }
}

impl FmtLabels for NoBudgetLabel {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "skipped=\"no_budget\"")
    }
}
