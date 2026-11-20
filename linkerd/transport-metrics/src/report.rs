use super::Metrics;
use super::{
    tcp_close_total, tcp_open_connections, tcp_open_total, tcp_read_bytes_total,
    tcp_write_bytes_total, EosMetrics, Inner,
};
use linkerd_metrics::legacy::{FmtLabels, FmtMetric, FmtMetrics, Metric, Snapshot};
use std::{fmt, hash::Hash, sync::Arc};
use tokio::time::{Duration, Instant};

/// Implements `FmtMetrics` to render prometheus-formatted metrics for all transports.
#[derive(Clone, Debug)]
pub struct Report<K: Eq + Hash + FmtLabels> {
    metrics: Arc<Inner<K>>,
    retain_idle: Duration,
}

// === impl Report ===

impl<K: Eq + Hash + FmtLabels> Report<K> {
    pub(super) fn new(metrics: Arc<Inner<K>>, retain_idle: Duration) -> Self {
        Self {
            metrics,
            retain_idle,
        }
    }
}

impl<K: Eq + Hash + FmtLabels + 'static> Report<K> {
    /// Formats a metric across all entries of a registry snapshot.
    fn fmt_by<N, M>(
        snapshot: &Snapshot<K, Metrics>,
        f: &mut fmt::Formatter<'_>,
        metric: Metric<'_, N, M>,
        get_metric: impl Fn(&Metrics) -> &M,
    ) -> fmt::Result
    where
        N: fmt::Display,
        M: FmtMetric,
    {
        for (key, m) in snapshot {
            get_metric(m).fmt_metric_labeled(f, &metric.name, key.as_ref())?;
        }

        Ok(())
    }

    /// Formats a metric across all instances of `EosMetrics` in the registry.
    fn fmt_eos_by<N, M>(
        inner: &Snapshot<K, Metrics>,
        f: &mut fmt::Formatter<'_>,
        metric: Metric<'_, N, M>,
        get_metric: impl Fn(&EosMetrics) -> &M,
    ) -> fmt::Result
    where
        N: fmt::Display,
        M: FmtMetric,
    {
        for (key, metrics) in inner {
            let key = key.as_ref();
            let by_eos = metrics.by_eos.lock();
            for (eos, m) in by_eos.metrics.iter() {
                get_metric(m).fmt_metric_labeled(f, &metric.name, (key, eos))?;
            }
        }

        Ok(())
    }
}

impl<K: Eq + Hash + FmtLabels + 'static> FmtMetrics for Report<K> {
    fn fmt_metrics(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.metrics.is_empty() {
            return Ok(());
        }

        {
            let snapshot = self.metrics.snapshot();

            tcp_open_total.fmt_help(f)?;
            Self::fmt_by(&snapshot, f, tcp_open_total, |m| &m.open_total)?;

            tcp_open_connections.fmt_help(f)?;
            Self::fmt_by(&snapshot, f, tcp_open_connections, |m| &m.open_connections)?;

            tcp_read_bytes_total.fmt_help(f)?;
            Self::fmt_by(&snapshot, f, tcp_read_bytes_total, |m| &m.read_bytes_total)?;

            tcp_write_bytes_total.fmt_help(f)?;
            Self::fmt_by(&snapshot, f, tcp_write_bytes_total, |m| {
                &m.write_bytes_total
            })?;

            tcp_close_total.fmt_help(f)?;
            Self::fmt_eos_by(&snapshot, f, tcp_close_total, |e| &e.close_total)?;
        }

        self.metrics.retain_since(Instant::now() - self.retain_idle);

        Ok(())
    }
}
