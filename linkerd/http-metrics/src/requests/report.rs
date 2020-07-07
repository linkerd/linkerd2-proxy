use super::{ClassMetrics, Metrics, StatusMetrics};
use crate::{Prefixed, Registry, Report};
use linkerd2_metrics::{latency, Counter, FmtLabels, FmtMetric, FmtMetrics, Histogram, Metric};
use std::fmt;
use std::hash::Hash;
use std::time::Instant;
use tracing::trace;

struct Status(http::StatusCode);

impl<T, C> Report<T, Metrics<C>>
where
    T: FmtLabels + Hash + Eq,
    C: FmtLabels + Hash + Eq,
{
    fn request_total(&self) -> Metric<'_, Prefixed<'_, &'static str>, Counter> {
        Metric::new(
            self.prefix_key("request_total"),
            "Total count of HTTP requests.",
        )
    }

    fn response_total(&self) -> Metric<'_, Prefixed<'_, &'static str>, Counter> {
        Metric::new(
            self.prefix_key("response_total"),
            "Total count of HTTP responses.",
        )
    }

    fn response_latency_ms(
        &self,
    ) -> Metric<'_, Prefixed<'_, &'static str>, Histogram<latency::Ms>> {
        Metric::new(
            self.prefix_key("response_latency_ms"),
            "Elapsed times between a request's headers being received \
             and its response stream completing",
        )
    }
}

impl<T, C> FmtMetrics for Report<T, Metrics<C>>
where
    T: FmtLabels + Hash + Eq,
    C: FmtLabels + Hash + Eq,
{
    fn fmt_metrics(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut registry = match self.registry.lock() {
            Err(_) => return Ok(()),
            Ok(r) => r,
        };
        trace!(
            prefix = self.prefix,
            targets = registry.by_target.len(),
            include_latencies = self.include_latencies,
            "Formatting HTTP request metrics",
        );

        if registry.by_target.is_empty() {
            return Ok(());
        }

        let metric = self.request_total();
        metric.fmt_help(f)?;
        registry.fmt_by_target(f, metric, |s| &s.total)?;

        if self.include_latencies {
            let metric = self.response_latency_ms();
            metric.fmt_help(f)?;
            registry.fmt_by_status(f, metric, |s| &s.latency)?;
        }

        let metric = self.response_total();
        metric.fmt_help(f)?;
        registry.fmt_by_class(f, metric, |s| &s.total)?;

        registry.retain_since(Instant::now() - self.retain_idle);

        Ok(())
    }
}

impl<T, C> Registry<T, Metrics<C>>
where
    T: FmtLabels + Hash + Eq,
    C: FmtLabels + Hash + Eq,
{
    fn fmt_by_target<N, V, F>(
        &self,
        f: &mut fmt::Formatter<'_>,
        metric: Metric<'_, N, V>,
        get_metric: F,
    ) -> fmt::Result
    where
        N: fmt::Display,
        V: FmtMetric,
        F: Fn(&Metrics<C>) -> &V,
    {
        for (tgt, tm) in &self.by_target {
            if let Ok(m) = tm.lock() {
                get_metric(&*m).fmt_metric_labeled(f, &metric.name, tgt)?;
            }
        }

        Ok(())
    }

    fn fmt_by_status<N, M, F>(
        &self,
        f: &mut fmt::Formatter<'_>,
        metric: Metric<'_, N, M>,
        get_metric: F,
    ) -> fmt::Result
    where
        N: fmt::Display,
        M: FmtMetric,
        F: Fn(&StatusMetrics<C>) -> &M,
    {
        for (tgt, tm) in &self.by_target {
            if let Ok(tm) = tm.lock() {
                for (status, m) in &tm.by_status {
                    let status = status.as_ref().map(|s| Status(*s));
                    let labels = (tgt, status);
                    get_metric(&*m).fmt_metric_labeled(f, &metric.name, labels)?;
                }
            }
        }

        Ok(())
    }

    fn fmt_by_class<N, M, F>(
        &self,
        f: &mut fmt::Formatter<'_>,
        metric: Metric<'_, N, M>,
        get_metric: F,
    ) -> fmt::Result
    where
        N: fmt::Display,
        M: FmtMetric,
        F: Fn(&ClassMetrics) -> &M,
    {
        for (tgt, tm) in &self.by_target {
            if let Ok(tm) = tm.lock() {
                for (status, sm) in &tm.by_status {
                    for (cls, m) in &sm.by_class {
                        let status = status.as_ref().map(|s| Status(*s));
                        let labels = (tgt, (status, cls));
                        get_metric(&*m).fmt_metric_labeled(f, &metric.name, labels)?;
                    }
                }
            }
        }

        Ok(())
    }
}

impl FmtLabels for Status {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "status_code=\"{}\"", self.0.as_u16())
    }
}
