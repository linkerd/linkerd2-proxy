use std::fmt;
use std::marker::PhantomData;
use std::{cmp, iter, slice};

use super::{Counter, Factor, FmtLabels, FmtMetric};

/// A series of latency values and counts.
#[derive(Debug)]
pub struct Histogram<V: Into<u64>, F = ()> {
    bounds: &'static Bounds,
    buckets: Box<[Counter<F>]>,

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
    // TODO: Implement Prometheus reset semantics correctly, taking into consideration
    //       that Prometheus represents this as `f64` and so there are only 52 significant
    //       bits.
    sum: Counter,

    _p: PhantomData<V>,
}

#[derive(Debug, PartialEq, Copy, Clone)]
pub enum Bucket {
    Le(f64),
    Inf,
}

/// A series of increasing Buckets values.
#[derive(Debug)]
pub struct Bounds(pub &'static [Bucket]);

/// Helper that lazily formats an `{K}="{V}"`" label.
struct Label<K: fmt::Display, V: fmt::Display>(K, V);

// ===== impl Histogram =====

impl<V: Into<u64>, F: Factor> Histogram<V, F> {
    pub fn new(bounds: &'static Bounds) -> Self {
        let mut buckets = Vec::with_capacity(bounds.0.len());
        let mut prior = &Bucket::Le(0.0);
        for bound in bounds.0.iter() {
            assert!(prior < bound);
            buckets.push(Counter::new());
            prior = bound;
        }

        Self {
            bounds,
            buckets: buckets.into_boxed_slice(),
            sum: Counter::default(),
            _p: PhantomData,
        }
    }

    pub fn add<U: Into<V>>(&self, u: U) {
        let v: V = u.into();
        let value: u64 = v.into();

        let idx = self
            .bounds
            .0
            .iter()
            .position(|b| match *b {
                Bucket::Le(ceiling) => F::factor(value) <= ceiling,
                Bucket::Inf => true,
            })
            .expect("all values must fit into a bucket");

        self.buckets[idx].incr();
        self.sum.add(value);
    }
}

#[cfg(any(test, feature = "test_util"))]
#[allow(clippy::float_cmp)]
impl<V: Into<u64>, F: Factor + std::fmt::Debug> Histogram<V, F> {
    /// Assert the bucket containing `le` has a count of at least `at_least`.
    pub fn assert_bucket_at_least(&self, le: f64, at_least: f64) {
        for (&bucket, count) in self {
            if bucket >= le {
                let count = count.value();
                assert!(count >= at_least, "le={:?}; bucket={:?};", le, bucket);
                break;
            }
        }
    }

    /// Assert the bucket containing `le` has a count of exactly `exactly`.
    pub fn assert_bucket_exactly(&self, le: f64, exactly: f64) -> &Self {
        for (&bucket, count) in self {
            if bucket >= le {
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
    pub fn assert_lt_exactly(&self, value: f64, exactly: f64) -> &Self {
        for (i, &bucket) in self.bounds.0.iter().enumerate() {
            let ceiling = match bucket {
                Bucket::Le(c) => c,
                Bucket::Inf => break,
            };
            let next = self
                .bounds
                .0
                .get(i + 1)
                .expect("Bucket::Le may not be the last in `bounds`!");

            if value <= ceiling || next >= &value {
                break;
            }

            let count: f64 = self.buckets[i].value();
            assert_eq!(count, exactly, "bucket={:?}; value={:?};", bucket, value,);
        }
        self
    }

    /// Assert all buckets greater than the one containing `value` have
    /// counts of exactly `exactly`.
    pub fn assert_gt_exactly(&self, value: f64, exactly: f64) -> &Self {
        // We set this to true after we've iterated past the first bucket
        // whose upper bound is >= `value`.
        let mut past_le = false;
        for (&bucket, count) in self {
            if bucket < value {
                continue;
            }

            if bucket >= value && !past_le {
                past_le = true;
                continue;
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

impl<'a, V: Into<u64>, F> IntoIterator for &'a Histogram<V, F> {
    type Item = (&'a Bucket, &'a Counter<F>);
    type IntoIter = iter::Zip<slice::Iter<'a, Bucket>, slice::Iter<'a, Counter<F>>>;

    fn into_iter(self) -> Self::IntoIter {
        self.bounds.0.iter().zip(self.buckets.iter())
    }
}

impl<V: Into<u64>, F: Factor> FmtMetric for Histogram<V, F> {
    const KIND: &'static str = "histogram";

    fn fmt_metric<N: fmt::Display>(&self, f: &mut fmt::Formatter<'_>, name: N) -> fmt::Result {
        let total = Counter::<F>::new();
        for (le, count) in self {
            total.add(count.into());
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
        let total = Counter::<F>::new();
        for (le, count) in self {
            total.add(count.into());
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

// ===== impl Label =====

impl<K: fmt::Display, V: fmt::Display> FmtLabels for Label<K, V> {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}=\"{}\"", self.0, self.1)
    }
}

// ===== impl Bucket =====

impl fmt::Display for Bucket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Bucket::Le(v) => write!(f, "{}", v),
            Bucket::Inf => write!(f, "+Inf"),
        }
    }
}

impl cmp::PartialOrd<Bucket> for Bucket {
    fn partial_cmp(&self, rhs: &Bucket) -> Option<cmp::Ordering> {
        match (self, rhs) {
            (Bucket::Inf, Bucket::Inf) => None,
            (Bucket::Le(s), _) if s.is_nan() => None,
            (_, Bucket::Le(r)) if r.is_nan() => None,
            (Bucket::Le(_), Bucket::Inf) => Some(cmp::Ordering::Less),
            (Bucket::Inf, Bucket::Le(_)) => Some(cmp::Ordering::Greater),
            (Bucket::Le(s), Bucket::Le(r)) => s.partial_cmp(r),
        }
    }
}

impl cmp::PartialEq<f64> for Bucket {
    fn eq(&self, rhs: &f64) -> bool {
        if let Bucket::Le(ref ceiling) = *self {
            ceiling == rhs
        } else {
            // `self` is `Bucket::Inf`.
            false
        }
    }
}

impl cmp::PartialOrd<f64> for Bucket {
    fn partial_cmp(&self, rhs: &f64) -> Option<cmp::Ordering> {
        if let Bucket::Le(ref ceiling) = *self {
            ceiling.partial_cmp(rhs)
        } else {
            // `self` is `Bucket::Inf`.
            Some(cmp::Ordering::Greater)
        }
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    use quickcheck::quickcheck;
    use std::collections::HashMap;

    static BOUNDS: &Bounds = &Bounds(&[
        Bucket::Le(0.010),
        Bucket::Le(0.020),
        Bucket::Le(0.030),
        Bucket::Le(0.040),
        Bucket::Le(0.050),
        Bucket::Le(0.060),
        Bucket::Le(0.070),
        Bucket::Le(0.080),
        Bucket::Le(0.090),
        Bucket::Le(0.100),
        Bucket::Le(0.200),
        Bucket::Le(0.300),
        Bucket::Le(0.400),
        Bucket::Le(0.500),
        Bucket::Le(0.600),
        Bucket::Le(0.700),
        Bucket::Le(0.800),
        Bucket::Le(0.900),
        Bucket::Le(1.000),
        Bucket::Le(2.000),
        Bucket::Le(3.000),
        Bucket::Le(4.000),
        Bucket::Le(5.000),
        Bucket::Le(6.000),
        Bucket::Le(7.000),
        Bucket::Le(8.000),
        Bucket::Le(9.000),
        Bucket::Le(10.000),
        Bucket::Le(20.000),
        Bucket::Le(30.000),
        Bucket::Le(40.000),
        Bucket::Le(50.000),
        Bucket::Le(60.000),
        Bucket::Le(70.000),
        Bucket::Le(80.000),
        Bucket::Le(90.000),
        Bucket::Le(100.000),
        Bucket::Le(200.000),
        Bucket::Le(300.000),
        Bucket::Le(400.000),
        Bucket::Le(500.000),
        Bucket::Le(600.000),
        Bucket::Le(700.000),
        Bucket::Le(800.000),
        Bucket::Le(900.000),
        Bucket::Le(1_000.000),
        Bucket::Inf,
    ]);

    quickcheck! {
        fn bucket_incremented(obs: u64) -> bool {
            let hist = Histogram::<u64>::new(BOUNDS);
            hist.add(obs);
            // The bucket containing `obs` must have count 1.
            hist.assert_bucket_exactly(obs as f64, 1.0)
                // All buckets less than the one containing `obs` must have
                // counts of exactly 0.
                .assert_lt_exactly(obs as f64, 0.0)
                // All buckets greater than the one containing `obs` must
                // have counts of exactly 0.
                .assert_gt_exactly(obs as f64, 0.0);
            true
        }

        fn sum_equals_total_of_observations(observations: Vec<u64>) -> bool {
            let hist = Histogram::<u64>::new(BOUNDS);

            let expected_sum = Counter::<()>::default();
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

            let count = hist.buckets.iter().map(|c| c.value()).sum::<f64>();
            count == observations.len() as f64
        }

        fn multiple_observations_increment_buckets(observations: Vec<u64>) -> bool {
            let mut buckets_and_counts: HashMap<usize, f64> = HashMap::new();
            let hist = Histogram::<u64>::new(BOUNDS);

            for obs in observations {
                let incremented_bucket = &BOUNDS.0.iter()
                    .position(|bucket| match *bucket {
                        Bucket::Le(ceiling) => obs as f64 <= ceiling,
                        Bucket::Inf => true,
                    })
                    .unwrap();
                *buckets_and_counts
                    .entry(*incremented_bucket)
                    .or_insert(0.0) += 1.0;

                hist.add(obs);
            }

            for (i, count) in hist.buckets.iter().enumerate() {
                let count = count.value();
                assert_eq!(buckets_and_counts.get(&i).unwrap_or(&0.0), &count);
            }
            true
        }
    }
}
