use crate::{
    prom::{FmtLabels, FmtMetric},
    value::{PromValue, Value},
};
use std::fmt::{self, Display};

/// A Prometheus counter is represented by a `Wrapping` unsigned 52-bit integer.
///
/// Counters always explicitly wrap to zero when value overflows 2^53.
/// This behavior is dictated by the fact that Prometheus represents counters
/// using 64-bit floating-point numbers, whose mantissa is 52-bit wide.
/// Prometheus' [`rate()`] and [`irate()`] queries handle breaks
/// in monotonicity gracefully  (see also [`resets()`]), so derived metrics
/// are still reliable on overflows.
///
/// [`rate()`]: https://prometheus.io/docs/prometheus/latest/querying/functions/#rate()
/// [`irate()`]: https://prometheus.io/docs/prometheus/latest/querying/functions/#irate()
/// [`resets()`]: https://prometheus.io/docs/prometheus/latest/querying/functions/#resets
#[derive(Debug)]
pub struct Counter<T = u64>(Value<T>);

// === impl Counter ===

impl<V> Default for Counter<V> {
    fn default() -> Self {
        Self(Value::default())
    }
}

impl<V> Counter<V> {
    pub fn new() -> Self {
        Self::default()
    }
}

impl From<u64> for Counter<u64> {
    fn from(n: u64) -> Self {
        Self(Value::from(n))
    }
}

impl From<f64> for Counter<f64> {
    fn from(n: f64) -> Self {
        Self(Value::from(n))
    }
}

impl Counter<u64> {
    pub fn incr(&self) {
        self.add(1)
    }

    pub fn add(&self, n: u64) {
        self.0.add(n)
    }

    pub fn value(&self) -> u64 {
        self.0.value()
    }
}

impl Counter<f64> {
    pub fn incr(&self) {
        self.add(1.0)
    }

    pub fn add(&self, value: f64) {
        self.0.add(value);
    }

    pub fn value(&self) -> f64 {
        self.0.value()
    }
}

impl<T> FmtMetric for Counter<T>
where
    Value<T>: PromValue,
{
    const KIND: &'static str = "counter";

    fn fmt_metric<N: Display>(&self, f: &mut fmt::Formatter<'_>, name: N) -> fmt::Result {
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
        N: Display,
    {
        write!(f, "{}{{", name)?;
        labels.fmt_labels(f)?;
        writeln!(f, "}} {}", self.0.prom_value())
    }
}

impl<T> PromValue for Counter<T>
where
    Value<T>: PromValue,
{
    #[inline]
    fn prom_value(&self) -> f64 {
        self.0.prom_value()
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;
    use crate::value::MAX_PRECISE_UINT64;

    #[test]
    fn count_simple() {
        let c = Counter::<u64>::default();
        assert_eq!(c.prom_value(), 0.0);
        c.incr();
        assert_eq!(c.prom_value(), 1.0);
        c.add(41);
        assert_eq!(c.prom_value(), 42.0);
        c.add(0);
        assert_eq!(c.prom_value(), 42.0);
    }

    #[test]
    fn count_wrapping() {
        let c = Counter::<u64>::from(MAX_PRECISE_UINT64 - 1);
        assert_eq!(c.prom_value(), (MAX_PRECISE_UINT64 - 1) as f64);
        c.incr();
        assert_eq!(c.prom_value(), MAX_PRECISE_UINT64 as f64);
        c.incr();
        assert_eq!(c.prom_value(), 0.0);
        c.incr();
        assert_eq!(c.prom_value(), 1.0);

        let max = Counter::<u64>::from(MAX_PRECISE_UINT64);
        assert_eq!(max.prom_value(), MAX_PRECISE_UINT64 as f64);
    }

    #[test]
    fn f64_add() {
        let c = Counter::<f64>::from(0.001);
        assert_eq!(c.prom_value(), 0.001);
        c.add(0.01);
        assert_eq!(c.prom_value(), 0.011);
        c.add(0.1);
        assert_eq!(c.prom_value(), 0.111);
        c.add(1.0);
        assert_eq!(c.prom_value(), 1.111);
    }

    #[test]
    fn f64_overflow() {
        let c = Counter::<f64>::from(std::f64::MAX);
        assert_eq!(c.prom_value(), std::f64::MAX);
        c.add(std::f64::MAX / 2.0);
        assert_eq!(c.prom_value(), 0.0);
    }
}
