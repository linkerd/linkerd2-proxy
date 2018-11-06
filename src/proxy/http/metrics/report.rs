use http;
use std::fmt;
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_timer::clock;

use metrics::{latency, Counter, FmtLabels, FmtMetric, FmtMetrics, Histogram, Metric};

use super::{ClassMetrics, Metrics, Registry, StatusMetrics};

/// Reports HTTP metrics for prometheus.
#[derive(Clone, Debug)]
pub struct Report<T, C>
where
    T: FmtLabels + Hash + Eq,
    C: FmtLabels + Hash + Eq,
{
    scope: Scope,
    registry: Arc<Mutex<Registry<T, C>>>,
    retain_idle: Duration,
}

struct Status(http::StatusCode);

#[derive(Clone, Debug)]
struct Scope {
    request_total_key: String,
    response_total_key: String,
    response_latency_ms_key: String,
}

// ===== impl Report =====

impl<T, C> Report<T, C>
where
    T: FmtLabels + Hash + Eq,
    C: FmtLabels + Hash + Eq,
{
    pub(super) fn new(retain_idle: Duration, registry: Arc<Mutex<Registry<T, C>>>) -> Self {
        Self {
            registry,
            retain_idle,
            scope: Scope::default(),
        }
    }

    pub fn with_prefix(self, prefix: &'static str) -> Self {
        if prefix.is_empty() {
            return self;
        }

        Self {
            scope: Scope::prefixed(prefix),
            .. self
        }
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

        self.scope.request_total().fmt_help(f)?;
        registry.fmt_by_target(f, self.scope.request_total(), |s| &s.total)?;

        self.scope.response_latency_ms().fmt_help(f)?;
        registry.fmt_by_status(f, self.scope.response_latency_ms(), |s| &s.latency)?;

        self.scope.response_total().fmt_help(f)?;
        registry.fmt_by_class(f, self.scope.response_total(), |s| &s.total)?;

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

    fn fmt_by_status<M, F>(
        &self,
        f: &mut fmt::Formatter,
        metric: Metric<M>,
        get_metric: F,
    ) -> fmt::Result
    where
        M: FmtMetric,
        F: Fn(&StatusMetrics<C>) -> &M,
    {
        for (tgt, tm) in &self.by_target {
            if let Ok(tm) = tm.lock() {
                for (status, m) in &tm.by_status {
                    let labels = (tgt, Status(*status));
                    get_metric(&*m).fmt_metric_labeled(f, metric.name, labels)?;
                }
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
                for (status, sm) in &tm.by_status {
                    for (cls, m) in &sm.by_class {
                        let labels = (tgt, (Status(*status), cls));
                        get_metric(&*m).fmt_metric_labeled(f, metric.name, labels)?;
                    }
                }
            }
        }

        Ok(())
    }
}

// === impl Scope ===

impl Default for Scope {
    fn default() -> Self {
        Self {
            request_total_key: "request_total".to_owned(),
            response_total_key: "response_total".to_owned(),
            response_latency_ms_key: "response_latency_ms".to_owned(),
        }
    }
}

impl Scope {
    fn prefixed(prefix: &'static str) -> Self {
        if prefix.is_empty() {
            return Self::default();
        }

        Self {
            request_total_key: format!("{}_request_total", prefix),
            response_total_key: format!("{}_response_total", prefix),
            response_latency_ms_key: format!("{}_response_latency_ms", prefix),
        }
    }

    fn request_total(&self) -> Metric<Counter> {
        Metric::new(&self.request_total_key, &Self::REQUEST_TOTAL_HELP)
    }

    fn response_total(&self) -> Metric<Counter> {
        Metric::new(&self.response_total_key, &Self::RESPONSE_TOTAL_HELP)
    }

    fn response_latency_ms(&self) -> Metric<Histogram<latency::Ms>> {
        Metric::new(&self.response_latency_ms_key, &Self::RESPONSE_LATENCY_MS_HELP)
    }

    const REQUEST_TOTAL_HELP: &'static str = "Total count of HTTP requests.";

    const RESPONSE_TOTAL_HELP: &'static str = "Total count of HTTP responses.";

    const RESPONSE_LATENCY_MS_HELP: &'static str =
        "Elapsed times between a request's headers being received \
        and its response stream completing";
}

impl FmtLabels for Status {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "status_code=\"{}\"", self.0.as_u16())
    }
}
