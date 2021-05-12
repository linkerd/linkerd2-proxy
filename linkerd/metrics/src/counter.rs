use super::{
    prom::{FmtLabels, FmtMetric},
    Factor,
};
use std::fmt::{self, Display};
use std::sync::atomic::{AtomicU64, Ordering};

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
pub struct Counter<F = ()>(AtomicU64, std::marker::PhantomData<F>);

// ===== impl Counter =====

impl<F> Default for Counter<F> {
    fn default() -> Self {
        Self(AtomicU64::default(), std::marker::PhantomData)
    }
}

impl<F> Counter<F> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn incr(&self) {
        self.add(1)
    }

    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Release);
    }
}

impl<F: Factor> Counter<F> {
    /// Return current counter value, wrapped to be safe for use with Prometheus.
    pub fn value(&self) -> f64 {
        let n = self.0.load(Ordering::Acquire);
        F::factor(n)
    }
}

impl<F: Factor> From<&Counter<F>> for f64 {
    fn from(counter: &Counter<F>) -> f64 {
        counter.value()
    }
}

impl<F> From<&Counter<F>> for u64 {
    fn from(Counter(ref counter, _): &Counter<F>) -> u64 {
        counter.load(Ordering::Acquire)
    }
}

impl<F> From<u64> for Counter<F> {
    fn from(value: u64) -> Self {
        Counter(value.into(), std::marker::PhantomData)
    }
}

impl<F: Factor> FmtMetric for Counter<F> {
    const KIND: &'static str = "counter";

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

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;
    use crate::{MicrosAsSeconds, MillisAsSeconds, MAX_PRECISE_UINT64};

    #[test]
    fn count_simple() {
        let c = Counter::<()>::default();
        assert_eq!(c.value(), 0.0);
        c.incr();
        assert_eq!(c.value(), 1.0);
        c.add(41);
        assert_eq!(c.value(), 42.0);
        c.add(0);
        assert_eq!(c.value(), 42.0);
    }

    #[test]
    fn count_wrapping() {
        let c = Counter::<()>::from(MAX_PRECISE_UINT64 - 1);
        assert_eq!(c.value(), (MAX_PRECISE_UINT64 - 1) as f64);
        c.incr();
        assert_eq!(c.value(), MAX_PRECISE_UINT64 as f64);
        c.incr();
        assert_eq!(c.value(), 0.0);
        c.incr();
        assert_eq!(c.value(), 1.0);

        let max = Counter::<()>::from(MAX_PRECISE_UINT64);
        assert_eq!(max.value(), MAX_PRECISE_UINT64 as f64);
    }

    #[test]
    fn millis_as_seconds() {
        let c = Counter::<MillisAsSeconds>::from(1);
        assert_eq!(c.value(), 0.001);

        let c = Counter::<MillisAsSeconds>::from((MAX_PRECISE_UINT64 - 1) * 1000);
        assert_eq!(c.value(), (MAX_PRECISE_UINT64 - 1) as f64);
        c.add(1000);
        assert_eq!(c.value(), MAX_PRECISE_UINT64 as f64);
        c.add(1000);
        assert_eq!(c.value(), 0.0);
        c.add(1000);
        assert_eq!(c.value(), 1.0);

        let max = Counter::<MillisAsSeconds>::from(MAX_PRECISE_UINT64 * 1000);
        assert_eq!(max.value(), MAX_PRECISE_UINT64 as f64);
    }

    #[test]
    fn micros_as_seconds() {
        let c = Counter::<MicrosAsSeconds>::from(1);
        assert_eq!(c.value(), 0.000_001);
        c.add(110);
        assert_eq!(c.value(), 0.000_111);

        let c = Counter::<MicrosAsSeconds>::from((MAX_PRECISE_UINT64 - 1) * 1000);
        assert_eq!(c.value(), (MAX_PRECISE_UINT64 - 1) as f64 * 0.001);
        c.add(1_000);
        assert_eq!(c.value(), MAX_PRECISE_UINT64 as f64 * 0.001);
        c.add(1_000);
        assert_eq!(c.value(), 0.0);
        c.add(1);
        assert_eq!(c.value(), 0.000_001);

        let max = Counter::<MicrosAsSeconds>::from(MAX_PRECISE_UINT64 * 1000);
        assert_eq!(max.value(), MAX_PRECISE_UINT64 as f64 * 0.001);
    }
}
