use crate::{
    value::{self, PromValue, Value},
    Counter, FmtLabels, FmtMetric,
};
use std::{fmt, iter, slice};

/// A series of latency values and counts.
#[derive(Debug)]
pub struct Histogram<T = u64> {
    bounds: &'static [f64],
    buckets: Box<[Counter]>,

    /// The total sum of all observed latency values.
    ///
    /// Histogram sums always explicitly wrap on overflows rather than
    /// panicking in debug builds. Prometheus' [`rate()`] and [`irate()`]
    /// queries handle breaks in monotonicity gracefully (see also
    /// [`resets()`]), so wrapping is less problematic than panicking in this
    /// case.
    ///
    /// Note, however, that Prometheus actually represents this using 64-bit
    /// floating-point numbers. The correct semantics are to ensure the sum
    /// always gets reset to zero after Prometheus reads it, before it would
    /// ever overflow a 52-bit `f64` mantissa.
    ///
    /// [`rate()`]: https://prometheus.io/docs/prometheus/latest/querying/functions/#rate()
    /// [`irate()`]: https://prometheus.io/docs/prometheus/latest/querying/functions/#irate()
    /// [`resets()`]: https://prometheus.io/docs/prometheus/latest/querying/functions/#resets
    ///
    // TODO: Implement Prometheus reset semantics correctly.
    sum: Counter<T>,
}

#[derive(Debug)]
pub enum Bucket {
    Le(f64),
    Inf,
}

/// Helper that lazily formats an `{K}="{V}"`" label.
struct Label<K: fmt::Display, V: fmt::Display>(K, V);

// === impl Histogram ===

impl<T> Histogram<T> {
    pub fn new(bounds: &'static [f64]) -> Self {
        let mut buckets = Vec::new();
        let mut prior = 0.0;
        for bound in bounds.iter() {
            assert!(prior < *bound);
            buckets.push(Counter::new());
            prior = *bound;
        }
        buckets.push(Counter::new());

        Self {
            bounds,
            buckets: buckets.into_boxed_slice(),
            sum: Counter::default(),
        }
    }
}

impl Histogram<u64> {
    pub fn add(&self, value: u64) {
        let wrapped = value::u64_to_f64(value);
        let idx = self
            .bounds
            .iter()
            .position(|b| wrapped <= *b)
            .unwrap_or(self.bounds.len());

        self.buckets[idx].incr();
        self.sum.add(value);
    }
}

impl Histogram<f64> {
    pub fn add(&self, value: f64) {
        let idx = self
            .bounds
            .iter()
            .position(|b| value <= *b)
            .unwrap_or(self.bounds.len());

        self.buckets[idx].incr();
        self.sum.add(value);
    }
}

#[cfg(any(test, feature = "test_util"))]
#[allow(clippy::float_cmp)]
impl<T> Histogram<T> {
    /// Assert the bucket containing `le` has a count of at least `at_least`.
    pub fn assert_bucket_at_least(&self, le: f64, at_least: u64) {
        for (bucket, count) in self {
            if bucket >= Bucket::Le(le) {
                let count = count.value();
                assert!(count >= at_least, "le={:?}; bucket={:?};", le, bucket);
                break;
            }
        }
    }

    /// Assert the bucket containing `le` has a count of exactly `exactly`.
    pub fn assert_bucket_exactly(&self, le: f64, exactly: u64) -> &Self {
        for (bucket, count) in self {
            if bucket >= Bucket::Le(le) {
                let count = count.value();
                assert_eq!(
                    count, exactly,
                    "le={:?}; bucket={:?}; buckets={:#?};",
                    le, bucket, self.buckets,
                );
                break;
            }
        }
        self
    }

    /// Assert all buckets less than the one containing `value` have
    /// counts of exactly `exactly`.
    pub fn assert_lt_exactly(&self, value: f64, exactly: u64) -> &Self {
        for (i, bucket) in self.bounds.iter().enumerate() {
            let next = self
                .bounds
                .get(i + 1)
                .map(|b| Bucket::Le(*b))
                .unwrap_or(Bucket::Inf);

            if value <= *bucket || next >= value {
                break;
            }

            let count = self.buckets[i].value();
            assert_eq!(count, exactly, "bucket={:?}; value={:?};", bucket, value);
        }
        self
    }

    /// Assert all buckets greater than the one containing `value` have
    /// counts of exactly `exactly`.
    pub fn assert_gt_exactly(&self, value: f64, exactly: u64) -> &Self {
        // We set this to true after we've iterated past the first bucket
        // whose upper bound is >= `value`.
        let mut past_le = false;
        for (bucket, count) in self {
            if let Bucket::Le(b) = bucket {
                if b < value {
                    continue;
                }

                if b >= value && !past_le {
                    past_le = true;
                    continue;
                }
            }

            if past_le {
                assert_eq!(
                    count.value(),
                    exactly,
                    "bucket={:?}; value={:?};",
                    bucket,
                    value,
                );
            }
        }
        self
    }
}

impl<'a, T> IntoIterator for &'a Histogram<T> {
    type Item = (Bucket, &'a Counter);
    type IntoIter = iter::Zip<
        iter::Chain<
            std::iter::Map<slice::Iter<'a, f64>, fn(&f64) -> Bucket>,
            std::iter::Once<Bucket>,
        >,
        slice::Iter<'a, Counter>,
    >;

    fn into_iter(self) -> Self::IntoIter {
        self.bounds
            .iter()
            .map::<_, fn(&f64) -> Bucket>(|n| Bucket::Le(*n))
            .chain(std::iter::once(Bucket::Inf))
            .zip(self.buckets.iter())
    }
}

impl<T> FmtMetric for Histogram<T>
where
    Value<T>: PromValue,
{
    const KIND: &'static str = "histogram";

    fn fmt_metric<N: fmt::Display>(&self, f: &mut fmt::Formatter<'_>, name: N) -> fmt::Result {
        let total = Counter::<u64>::new();
        for (le, count) in self {
            total.add(count.value());
            total.fmt_metric_labeled(f, format_args!("{}_bucket", &name), Label("le", le))?;
        }
        total.fmt_metric(f, format_args!("{}_count", &name))?;
        self.sum.fmt_metric(f, format_args!("{}_sum", &name))?;
        Ok(())
    }

    fn fmt_metric_labeled<N, L>(
        &self,
        f: &mut fmt::Formatter<'_>,
        name: N,
        labels: L,
    ) -> fmt::Result
    where
        N: fmt::Display,
        L: FmtLabels,
    {
        let total = Counter::<f64>::new();
        for (le, count) in self {
            total.add(count.prom_value());
            total.fmt_metric_labeled(
                f,
                format_args!("{}_bucket", &name),
                (&labels, Label("le", le)),
            )?;
        }
        total.fmt_metric_labeled(f, format_args!("{}_count", &name), &labels)?;
        self.sum
            .fmt_metric_labeled(f, format_args!("{}_sum", &name), &labels)?;
        Ok(())
    }
}

// === impl Label ===

impl<K: fmt::Display, V: fmt::Display> FmtLabels for Label<K, V> {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}=\"{}\"", self.0, self.1)
    }
}

// === impl Bucket ===

impl std::cmp::PartialEq<Bucket> for Bucket {
    fn eq(&self, other: &Bucket) -> bool {
        match (self, other) {
            (Self::Le(a), Self::Le(b)) => a.eq(b),
            (Self::Inf, Self::Inf) => true,
            _ => false,
        }
    }
}

impl std::cmp::PartialOrd<Bucket> for Bucket {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (Self::Le(a), Self::Le(b)) => a.partial_cmp(b),
            (Self::Le(_), Self::Inf) => Some(std::cmp::Ordering::Less),
            (Self::Inf, Self::Le(_)) => Some(std::cmp::Ordering::Greater),
            (Self::Inf, Self::Inf) => Some(std::cmp::Ordering::Equal),
        }
    }
}

impl std::cmp::PartialEq<f64> for Bucket {
    fn eq(&self, other: &f64) -> bool {
        match (self, other) {
            (Self::Le(a), b) => a.eq(b),
            _ => false,
        }
    }
}

impl std::cmp::PartialOrd<f64> for Bucket {
    fn partial_cmp(&self, other: &f64) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (Self::Le(a), b) => a.partial_cmp(b),
            (Self::Inf, _) => Some(std::cmp::Ordering::Greater),
        }
    }
}

impl std::fmt::Display for Bucket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Le(le) => write!(f, "{le}"),
            Self::Inf => write!(f, "+Inf"),
        }
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;
    use quickcheck::quickcheck;
    use std::collections::HashMap;

    static BOUNDS: &[f64] = &[
        0.010, 0.020, 0.030, 0.040, 0.050, 0.060, 0.070, 0.080, 0.090, 0.100, 0.200, 0.300, 0.400,
        0.500, 0.600, 0.700, 0.800, 0.900, 1.000, 2.000, 3.000, 4.000, 5.000, 6.000, 7.000, 8.000,
        9.000, 10.000, 20.000, 30.000, 40.000, 50.000, 60.000, 70.000, 80.000, 90.000, 100.000,
        200.000, 300.000, 400.000, 500.000, 600.000, 700.000, 800.000, 900.000, 1_000.000,
    ];

    quickcheck! {
        fn bucket_incremented(obs: u64) -> bool {
            let hist = Histogram::<u64>::new(BOUNDS);
            hist.add(obs);
            // The bucket containing `obs` must have count 1.
            hist.assert_bucket_exactly(obs as f64, 1)
                // All buckets less than the one containing `obs` must have
                // counts of exactly 0.
                .assert_lt_exactly(obs as f64, 0)
                // All buckets greater than the one containing `obs` must
                // have counts of exactly 0.
                .assert_gt_exactly(obs as f64, 0);
            true
        }

        fn sum_equals_total_of_observations(observations: Vec<u64>) -> bool {
            let hist = Histogram::<u64>::new(BOUNDS);

            let expected_sum = Counter::<u64>::default();
            for obs in observations {
                expected_sum.add(obs);
                hist.add(obs);
            }

            hist.sum.value() == expected_sum.value()
        }


        fn sum_equals_total_of_observations_f64(observations: Vec<f64>) -> bool {
            let hist = Histogram::<f64>::new(BOUNDS);

            let expected_sum = Counter::<f64>::default();
            for obs in observations {
                expected_sum.add(obs);
                hist.add(obs);
            }

            hist.sum.value() == expected_sum.value()
        }

        fn count_equals_number_of_observations(observations: Vec<u64>) -> bool {
            let hist = Histogram::<u64>::new(BOUNDS);

            for obs in &observations {
                hist.add(*obs);
            }

            let count = hist.buckets.iter().map(|c| c.value()).sum::<u64>();
            count == observations.len() as u64
        }

        fn multiple_observations_increment_buckets(observations: Vec<u64>) -> bool {
            let mut buckets_and_counts: HashMap<usize, f64> = HashMap::new();
            let hist = Histogram::<u64>::new(BOUNDS);

            for obs in observations {
                let incremented_bucket = &BOUNDS.iter()
                    .position(|bucket| obs as f64 <= *bucket)
                    .unwrap_or(BOUNDS.len());
                *buckets_and_counts
                    .entry(*incremented_bucket)
                    .or_insert(0.0) += 1.0;

                hist.add(obs);
            }

            for (i, count) in hist.buckets.iter().enumerate() {
                let count = count.value() as f64;
                assert_eq!(buckets_and_counts.get(&i).unwrap_or(&0.0), &count);
            }
            true
        }
    }

    #[test]
    fn test_values_larger_than_largest_bound() {
        let hist = Histogram::<f64>::new(&[0.5]);
        assert_eq!(hist.bounds.len(), 1);
        assert_eq!(hist.buckets.len(), 2);
        assert_eq!(hist.buckets[0].value(), 0);
        assert_eq!(hist.buckets[1].value(), 0);

        hist.add(0.4);
        assert_eq!(hist.buckets[0].value(), 1);
        assert_eq!(hist.buckets[1].value(), 0);

        hist.add(0.6);
        assert_eq!(hist.buckets[0].value(), 1);
        assert_eq!(hist.buckets[1].value(), 1);
    }
}
