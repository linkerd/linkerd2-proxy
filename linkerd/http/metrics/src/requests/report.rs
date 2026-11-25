use super::{ClassMetrics, Metrics, StatusMetrics};
use crate::{Prefixed, Report};
use linkerd_metrics::{
    latency,
    legacy::{Counter, FmtLabels, FmtMetric, FmtMetrics, Histogram, Metric, Snapshot},
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
        snapshot: &Snapshot<T, Mutex<Metrics<C>>>,
        f: &mut fmt::Formatter<'_>,
        metric: Metric<'_, N, M>,
        get_metric: impl Fn(&Metrics<C>) -> &M,
    ) -> fmt::Result
    where
        N: fmt::Display,
        M: FmtMetric,
    {
        for (tgt, tm) in snapshot {
            let tm = tm.lock();
            get_metric(&tm).fmt_metric_labeled(f, &metric.name, tgt.as_ref())?;
        }

        Ok(())
    }

    fn fmt_by_status<N, M>(
        snapshot: &Snapshot<T, Mutex<Metrics<C>>>,
        f: &mut fmt::Formatter<'_>,
        metric: Metric<'_, N, M>,
        get_metric: impl Fn(&StatusMetrics<C>) -> &M,
    ) -> fmt::Result
    where
        N: fmt::Display,
        M: FmtMetric,
    {
        for (tgt, tm) in snapshot {
            let tgt = tgt.as_ref();
            let tm = tm.lock();
            for (status, m) in &tm.by_status {
                let status = status.as_ref().map(|s| Status(*s));
                let labels = (tgt, status);
                get_metric(m).fmt_metric_labeled(f, &metric.name, labels)?;
            }
        }

        Ok(())
    }

    fn fmt_by_class<N, M>(
        snapshot: &Snapshot<T, Mutex<Metrics<C>>>,
        f: &mut fmt::Formatter<'_>,
        metric: Metric<'_, N, M>,
        get_metric: impl Fn(&ClassMetrics) -> &M,
    ) -> fmt::Result
    where
        N: fmt::Display,
        M: FmtMetric,
    {
        for (tgt, tm) in snapshot {
            let tgt = tgt.as_ref();
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
        trace!(
            prefix = self.prefix,
            targets = self.registry.len(),
            include_latencies = self.include_latencies,
            "Formatting HTTP request metrics",
        );

        if self.registry.is_empty() {
            return Ok(());
        }

        {
            // Copy the entries out, then format without holding any store lock. The snapshot is
            // dropped at the end of this block so that eviction sees the real reference counts.
            let snapshot = self.registry.snapshot();

            let metric = self.request_total();
            metric.fmt_help(f)?;
            Self::fmt_by_target(&snapshot, f, metric, |s| &s.total)?;

            if self.include_latencies {
                let metric = self.response_latency_ms();
                metric.fmt_help(f)?;
                Self::fmt_by_status(&snapshot, f, metric, |s| &s.latency)?;
            }

            let metric = self.response_total();
            metric.fmt_help(f)?;
            Self::fmt_by_class(&snapshot, f, metric, |s| &s.total)?;
        }

        self.registry
            .retain_since(Instant::now() - self.retain_idle);

        Ok(())
    }
}

impl FmtLabels for Status {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self(status) = self;
        write!(f, "status_code=\"{}\"", status.as_u16())
    }
}
