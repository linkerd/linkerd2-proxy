use crate::{
    prom::{FmtLabels, FmtMetric},
    value::{PromValue, Value},
};
use std::fmt;

/// An instaneous metric value.
#[derive(Debug, Default)]
pub struct Gauge<T = u64>(Value<T>);

// === impl Gauge ===

impl From<u64> for Gauge<u64> {
    fn from(n: u64) -> Self {
        Self(n.into())
    }
}

impl From<f64> for Gauge<f64> {
    fn from(n: f64) -> Self {
        Self(n.into())
    }
}

impl Gauge<u64> {
    pub fn incr(&self) {
        self.0.add(1);
    }

    pub fn decr(&self) {
        self.0.sub(1);
    }

    pub fn set(&self, v: u64) {
        self.0.set(v)
    }

    pub fn value(&self) -> u64 {
        self.0.value()
    }
}

impl Gauge<f64> {
    pub fn set(&self, v: f64) {
        self.0.set(v)
    }

    pub fn value(&self) -> f64 {
        self.0.value()
    }
}

impl<T> FmtMetric for Gauge<T>
where
    Value<T>: PromValue,
{
    const KIND: &'static str = "gauge";

    fn fmt_metric<N: fmt::Display>(&self, f: &mut fmt::Formatter<'_>, name: N) -> fmt::Result {
        writeln!(f, "{} {}", name, self.0.prom_value())
    }

    fn fmt_metric_labeled<N, L>(
        &self,
        f: &mut fmt::Formatter<'_>,
        name: N,
        labels: L,
    ) -> fmt::Result
    where
        L: FmtLabels,
        N: fmt::Display,
    {
        write!(f, "{}{{", name)?;
        labels.fmt_labels(f)?;
        writeln!(f, "}} {}", self.0.prom_value())
    }
}
