use super::{
    tcp_close_total, tcp_open_connections, tcp_open_total, tcp_read_bytes_total,
    tcp_write_bytes_total, EosMetrics, Inner,
};
use linkerd_metrics::{FmtLabels, FmtMetric, FmtMetrics, Metric};
use parking_lot::Mutex;
use std::{
    fmt,
    hash::Hash,
    sync::Arc,
    time::{Duration, Instant},
};

/// Implements `FmtMetrics` to render prometheus-formatted metrics for all transports.
#[derive(Clone, Debug)]
pub struct Report<K: Eq + Hash + FmtLabels> {
    metrics: Arc<Mutex<Inner<K>>>,
    retain_idle: Duration,
}

// === impl Report ===

impl<K: Eq + Hash + FmtLabels> Report<K> {
    pub(super) fn new(metrics: Arc<Mutex<Inner<K>>>, retain_idle: Duration) -> Self {
        Self {
            metrics,
            retain_idle,
        }
    }
}

impl<K: Eq + Hash + FmtLabels + 'static> Report<K> {
    /// Formats a metric across all instances of `EosMetrics` in the registry.
    fn fmt_eos_by<N, M>(
        inner: &Inner<K>,
        f: &mut fmt::Formatter<'_>,
        metric: Metric<'_, N, M>,
        get_metric: impl Fn(&EosMetrics) -> &M,
    ) -> fmt::Result
    where
        N: fmt::Display,
        M: FmtMetric,
    {
        for (key, metrics) in inner.iter() {
            let by_eos = (*metrics).by_eos.lock();
            for (eos, m) in by_eos.metrics.iter() {
                get_metric(&*m).fmt_metric_labeled(f, &metric.name, (key, eos))?;
            }
        }

        Ok(())
    }
}

impl<K: Eq + Hash + FmtLabels + 'static> FmtMetrics for Report<K> {
    fn fmt_metrics(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut metrics = self.metrics.lock();
        if metrics.is_empty() {
            return Ok(());
        }

        tcp_open_total.fmt_help(f)?;
        metrics.fmt_by(f, tcp_open_total, |m| &m.open_total)?;

        tcp_open_connections.fmt_help(f)?;
        metrics.fmt_by(f, tcp_open_connections, |m| &m.open_connections)?;

        tcp_read_bytes_total.fmt_help(f)?;
        metrics.fmt_by(f, tcp_read_bytes_total, |m| &m.read_bytes_total)?;

        tcp_write_bytes_total.fmt_help(f)?;
        metrics.fmt_by(f, tcp_write_bytes_total, |m| &m.write_bytes_total)?;

        tcp_close_total.fmt_help(f)?;
        Self::fmt_eos_by(&*metrics, f, tcp_close_total, |e| &e.close_total)?;

        metrics.retain_since(Instant::now().saturating_duration_since(self.retain_idle));

        Ok(())
    }
}
