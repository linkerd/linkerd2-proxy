use super::{ClassMetrics, Metrics};
use crate::{Prefixed, Report};
use linkerd_metrics::{
    latency, Counter, FmtLabels, FmtMetric, FmtMetrics, Histogram, Metric, Store,
};
use parking_lot::Mutex;
use std::{fmt, hash::Hash};
use tokio::time::Instant;
use tracing::trace;

#[derive(Copy, Clone)]
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

impl<T, C> Report<T, Metrics<C>>
where
    T: FmtLabels + Hash + Eq,
    C: FmtLabels + Hash + Eq,
{
    fn fmt_by_target<N, M>(
        registry: &Store<T, Mutex<Metrics<C>>>,
        f: &mut fmt::Formatter<'_>,
        metric: Metric<'_, N, M>,
        get_metric: impl Fn(&Metrics<C>) -> &M,
    ) -> fmt::Result
    where
        N: fmt::Display,
        M: FmtMetric,
    {
        registry.fmt_by_locked(f, metric, get_metric)
    }

    fn fmt_by_class<N, M>(
        registry: &Store<T, Mutex<Metrics<C>>>,
        f: &mut fmt::Formatter<'_>,
        metric: Metric<'_, N, M>,
        get_metric: impl Fn(&ClassMetrics) -> &M,
    ) -> fmt::Result
    where
        N: fmt::Display,
        M: FmtMetric,
    {
        for (tgt, tm) in registry.iter() {
            let tm = tm.lock();
            for (status, sm) in &tm.by_status {
                for (cls, m) in &sm.by_class {
                    let status = status.as_ref().map(|s| Status(*s));
                    let labels = (tgt, (status, cls));
                    get_metric(m).fmt_metric_labeled(f, &metric.name, labels)?;
                }
            }
        }

        Ok(())
    }
}

impl<T, C> FmtMetrics for Report<T, Metrics<C>>
where
    T: FmtLabels + Hash + Eq,
    C: FmtLabels + Hash + Eq,
{
    fn fmt_metrics(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut registry = self.registry.lock();
        trace!(
            prefix = self.prefix,
            targets = registry.len(),
            include_latencies = self.include_latencies,
            "Formatting HTTP request metrics",
        );

        if registry.is_empty() {
            return Ok(());
        }

        let metric = self.request_total();
        metric.fmt_help(f)?;
        Self::fmt_by_target(&registry, f, metric, |s| &s.total)?;

        if self.include_latencies {
            let metric = self.response_latency_ms();
            metric.fmt_help(f)?;
            Self::fmt_by_class(&registry, f, metric, |c| &c.latency)?;
        }

        let metric = self.response_total();
        metric.fmt_help(f)?;
        Self::fmt_by_class(&registry, f, metric, |s| &s.total)?;

        registry.retain_since(Instant::now() - self.retain_idle);

        Ok(())
    }
}

impl FmtLabels for Status {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "status_code=\"{}\"", self.0.as_u16())
    }
}
