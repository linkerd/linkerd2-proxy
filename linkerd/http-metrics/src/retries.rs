use super::{Prefixed, Report};
use linkerd2_metrics::{
    store::{LastUpdate, Registry, UpdatedAt},
    Counter, FmtLabels, FmtMetric, FmtMetrics, Metric,
};
use std::fmt;
use std::hash::Hash;
use std::sync::Arc;
use std::time::Duration;
use tracing::trace;

#[derive(Debug)]
pub struct Retries<T>(Registry<T, Metrics>)
where
    T: Hash + Eq;

#[derive(Clone, Debug)]
pub struct Handle(Arc<Metrics>);

#[derive(Debug)]
pub struct Metrics {
    clock: quanta::Clock,
    last_update: UpdatedAt,
    retryable: Counter,
    no_budget: Counter,
}

struct NoBudgetLabel;

// === impl Retries ===

impl<T: Hash + Eq> From<quanta::Clock> for Retries<T> {
    fn from(clock: quanta::Clock) -> Self {
        Retries(Registry::new(clock))
    }
}

impl<T: Hash + Eq> Retries<T> {
    pub fn into_report(self, retain_idle: Duration) -> Report<T, Metrics> {
        Report::new(retain_idle, self.0)
    }

    pub fn get_handle(&self, target: impl Into<T>) -> Handle {
        Handle(self.0.get_or_insert(target.into()))
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
        let m = &self.0;
        m.last_update.update(m.clock.recent());
        m.retryable.incr();
        if !has_budget {
            m.no_budget.incr();
        }
    }
}

// === impl Metrics ===

impl From<quanta::Clock> for Metrics {
    fn from(clock: quanta::Clock) -> Self {
        let last_update = UpdatedAt::new(&clock);
        Self {
            last_update,
            clock,
            retryable: Counter::default(),
            no_budget: Counter::default(),
        }
    }
}

impl LastUpdate for Metrics {
    fn last_update(&self) -> quanta::Instant {
        self.last_update.last_update(&self.clock)
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
        let registry = self.registry.read();
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
        for (tgt, m) in registry.iter() {
            m.retryable.fmt_metric_labeled(f, &metric.name, tgt)?;
            m.no_budget
                .fmt_metric_labeled(f, &metric.name, (tgt, NoBudgetLabel))?;
        }

        drop(registry);
        self.registry.write().retain_active(self.retain_idle);

        Ok(())
    }
}

impl FmtLabels for NoBudgetLabel {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "skipped=\"no_budget\"")
    }
}
