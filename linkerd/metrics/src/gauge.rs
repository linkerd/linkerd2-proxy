use super::prom::{FmtLabels, FmtMetric};
use std::fmt::{self, Display};
use std::sync::atomic::{AtomicU64, Ordering};

/// An instaneous metric value.
#[derive(Debug, Default)]
pub struct Gauge(AtomicU64);

impl Gauge {
    /// Increment the gauge by one.
    pub fn incr(&self) {
        self.0.fetch_add(1, Ordering::Release);
    }

    /// Decrement the gauge by one.
    pub fn decr(&self) {
        self.0.fetch_sub(1, Ordering::Release);
    }

    pub fn value(&self) -> u64 {
        self.0
            .load(Ordering::Acquire)
            .wrapping_rem(crate::MAX_PRECISE_UINT64 + 1)
    }
}

impl From<u64> for Gauge {
    fn from(n: u64) -> Self {
        Gauge(n.into())
    }
}

impl From<Gauge> for u64 {
    fn from(gauge: Gauge) -> u64 {
        gauge.value()
    }
}

impl FmtMetric for Gauge {
    const KIND: &'static str = "gauge";

    fn fmt_metric<N: Display>(&self, f: &mut fmt::Formatter<'_>, name: N) -> fmt::Result {
        writeln!(f, "{} {}", name, self.value())
    }

    fn fmt_metric_labeled<N, L>(
        &self,
        f: &mut fmt::Formatter<'_>,
        name: N,
        labels: L,
    ) -> fmt::Result
    where
        L: FmtLabels,
        N: Display,
    {
        write!(f, "{}{{", name)?;
        labels.fmt_labels(f)?;
        writeln!(f, "}} {}", self.value())
    }
}
