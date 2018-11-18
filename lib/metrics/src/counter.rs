use std::fmt::{self, Display};
use std::ops;

use super::prom::{FmtLabels, FmtMetric};

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
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Counter(u64);

/// Largest `u64` that can fit without loss of precision in `f64` (2^53).
pub(crate) const MAX_PRECISE_COUNTER: u64 = 0x20_0000_0000_0000;

// ===== impl Counter =====

impl Counter {
    /// Increment the counter by one.
    ///
    /// This function wraps on 52-bit overflows.
    pub fn incr(&mut self) {
        *self += 1;
    }

    /// Return current counter value.
    pub fn value(&self) -> u64 {
        self.0
    }
}

impl Into<u64> for Counter {
    fn into(self) -> u64 {
        self.0
    }
}

impl From<u64> for Counter {
    fn from(value: u64) -> Self {
        Counter(0) + value
    }
}

impl ops::Add<u64> for Counter {
    type Output = Self;
    fn add(self, rhs: u64) -> Self::Output {
        let wrapped = self
            .0
            .wrapping_add(rhs)
            .wrapping_rem(MAX_PRECISE_COUNTER + 1);
        Counter(wrapped)
    }
}

impl ops::Add<Self> for Counter {
    type Output = Self;
    fn add(self, Counter(rhs): Self) -> Self::Output {
        self + rhs
    }
}

impl ops::AddAssign<u64> for Counter {
    fn add_assign(&mut self, rhs: u64) {
        *self = *self + rhs
    }
}

impl ops::AddAssign<Self> for Counter {
    fn add_assign(&mut self, Counter(rhs): Self) {
        *self += rhs
    }
}

impl FmtMetric for Counter {
    const KIND: &'static str = "counter";

    fn fmt_metric<N: Display>(&self, f: &mut fmt::Formatter, name: N) -> fmt::Result {
        writeln!(f, "{} {}", name, self.0)
    }

    fn fmt_metric_labeled<N, L>(&self, f: &mut fmt::Formatter, name: N, labels: L) -> fmt::Result
    where
        L: FmtLabels,
        N: Display,
    {
        write!(f, "{}{{", name)?;
        labels.fmt_labels(f)?;
        writeln!(f, "}} {}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_simple() {
        let mut cnt = Counter::from(0);
        assert_eq!(cnt.value(), 0);
        cnt.incr();
        assert_eq!(cnt.value(), 1);
        cnt += 41;
        assert_eq!(cnt.value(), 42);
        cnt += 0;
        assert_eq!(cnt.value(), 42);
    }

    #[test]
    fn count_wrapping() {
        let mut cnt = Counter::from(MAX_PRECISE_COUNTER - 1);
        assert_eq!(cnt.value(), MAX_PRECISE_COUNTER - 1);
        cnt.incr();
        assert_eq!(cnt.value(), MAX_PRECISE_COUNTER);
        assert_eq!(cnt + 1, Counter::from(0));
        cnt.incr();
        assert_eq!(cnt.value(), 0);

        let max = Counter::from(MAX_PRECISE_COUNTER);
        assert_eq!(max.value(), MAX_PRECISE_COUNTER);

        let over = Counter::from(MAX_PRECISE_COUNTER + 1);
        assert_eq!(over.value(), 0);
    }
}
