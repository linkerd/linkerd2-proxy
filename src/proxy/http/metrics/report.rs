use std::fmt;
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_timer::clock;

use metrics::{latency, Counter, FmtLabels, FmtMetric, FmtMetrics, Histogram, Metric};

use super::{ClassMetrics, Metrics, Registry};

metrics! {
    request_total: Counter { "Total count of HTTP requests." },
    response_total: Counter { "Total count of HTTP responses" },
    response_latency_ms: Histogram<latency::Ms> {
        "Elapsed times between a request's headers being received \
        and its response stream completing"
    }
}

/// Reports HTTP metrics for prometheus.
#[derive(Clone, Debug)]
pub struct Report<T, C>
where
    T: FmtLabels + Hash + Eq,
    C: FmtLabels + Hash + Eq,
{
    registry: Arc<Mutex<Registry<T, C>>>,
    retain_idle: Duration,
}

// ===== impl Report =====

impl<T, C> Report<T, C>
where
    T: FmtLabels + Hash + Eq,
    C: FmtLabels + Hash + Eq,
{
    pub(super) fn new(retain_idle: Duration, registry: Arc<Mutex<Registry<T, C>>>) -> Self {
        Self { registry, retain_idle, }
    }
}

impl<T, C> FmtMetrics for Report<T, C>
where
    T: FmtLabels + Hash + Eq,
    C: FmtLabels + Hash + Eq,
{
    fn fmt_metrics(&self, f: &mut fmt::Formatter) -> fmt::Result {
        debug!("fmt_metrics");
        let mut registry = match self.registry.lock() {
            Err(_) => return Ok(()),
            Ok(r) => r,
        };

        let now = clock::now();
        let since = now - self.retain_idle;
        debug!("fmt_metrics: retain_since: now={:?} since={:?}", now, since);
        registry.retain_since(since);

        let registry = registry;
        debug!("fmt_metrics: by_target={}", registry.by_target.len());
        if registry.by_target.is_empty() {
            return Ok(());
        }

        request_total.fmt_help(f)?;
        registry.fmt_by_target(f, request_total, |s| &s.total)?;

        response_total.fmt_help(f)?;
        registry.fmt_by_class(f, response_total, |s| &s.total)?;
        //registry.fmt_by_target(f, response_total, |s| &s.unclassified.total)?;

        response_latency_ms.fmt_help(f)?;
        registry.fmt_by_class(f, response_latency_ms, |s| &s.latency)?;
        // registry.fmt_by_target(f, response_latency_ms, |s| {
        //     &s.unclassified.latency
        // })?;

        Ok(())
    }
}

impl<T, C> Registry<T, C>
where
    T: FmtLabels + Hash + Eq,
    C: FmtLabels + Hash + Eq,
{
    fn fmt_by_target<M, F>(
        &self,
        f: &mut fmt::Formatter,
        metric: Metric<M>,
        get_metric: F,
    ) -> fmt::Result
    where
        M: FmtMetric,
        F: Fn(&Metrics<C>) -> &M,
    {
        for (tgt, tm) in &self.by_target {
            if let Ok(m) = tm.lock() {
                get_metric(&*m).fmt_metric_labeled(f, metric.name, tgt)?;
            }
        }

        Ok(())
    }

    fn fmt_by_class<M, F>(
        &self,
        f: &mut fmt::Formatter,
        metric: Metric<M>,
        get_metric: F,
    ) -> fmt::Result
    where
        M: FmtMetric,
        F: Fn(&ClassMetrics) -> &M,
    {
        for (tgt, tm) in &self.by_target {
            if let Ok(tm) = tm.lock() {
                for (cls, m) in &tm.by_class {
                    let labels = (tgt, cls);
                    get_metric(&*m).fmt_metric_labeled(f, metric.name, labels)?;
                }
            }
        }

        Ok(())
    }
}
