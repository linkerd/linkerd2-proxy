use crate::{Counter, Factor, FmtLabels, FmtMetric};
pub use hdrhistogram::{AdditionError, CreationError, Histogram, RecordError};
use parking_lot::{Mutex, MutexGuard};
use std::fmt;
use tokio::time;

/// Summarizes a distribution of values at fixed quantiles over a sliding window.
#[derive(Debug)]
pub struct Summary<F = ()> {
    rotate_interval: time::Duration,
    next_rotate: time::Instant,

    window_idx: usize,
    windows: Vec<Histogram<u64>>,

    /// Instead of allocating a new histogram each time a report is formatted, we
    /// hold a single report and clear/repopulate it from the active windows.
    report: Mutex<Histogram<u64>>,
    quantiles: Vec<f64>,

    /// Count is tracked independently of the histogams so that rotated values
    /// are included.
    count: Counter,
    sum: Counter<F>,
}

/// Helper that lazily formats `quantile` labels.
struct FmtQuantile<'q>(&'q f64);

// === impl Summary ===

impl<F> Summary<F> {
    const DEFAULT_QUANTILES: [f64; 7] = [0.0, 0.50, 0.75, 0.90, 0.99, 0.999, 1.0];

    /// Creates a new summary with the specified number of windows. Values are
    /// are included for at most `lifetime`.
    ///
    /// A maximum value is required so that histograms can be merged
    /// deterministically. Values that exceed this maximum will be clamed, so
    /// summaries can hide outliers that exceed this maximum.
    ///
    /// A higher number of windows increases granularity at the cost of memory.
    /// For example, with `n_windows=2` and `liftime=1m`, the summary will rotate
    /// its histograms at most ever 30s. Whereas,with `n_windows=6`, histograms
    /// are rotated every 10s.
    pub fn new(
        n_windows: u32,
        lifetime: time::Duration,
        high: u64,
        sigfig: u8,
    ) -> Result<Self, CreationError> {
        // Create several histograms.
        debug_assert!(n_windows > 0);
        let mut windows = Vec::with_capacity(n_windows as usize);
        for _ in 0..n_windows {
            let h = Histogram::new_with_max(high, sigfig)?;
            windows.push(h.clone());
        }

        let report = {
            let r = Histogram::new_with_max(high, sigfig)?;
            Mutex::new(r)
        };

        let rotate_interval = lifetime / n_windows;
        let next_rotate = time::Instant::now() + rotate_interval;

        Ok(Self {
            windows,
            window_idx: 0,

            rotate_interval,
            next_rotate,

            report,
            quantiles: Vec::from(Self::DEFAULT_QUANTILES.clone()),

            count: Counter::new(),
            sum: Counter::new(),
        })
    }

    /// Overrides the default quantiles for this summary.
    pub fn with_quantiles(mut self, qs: impl IntoIterator<Item = f64>) -> Self {
        self.quantiles = qs.into_iter().collect();
        self
    }

    /// Record a value in the current histogram.
    ///
    /// The current histogram is updated as needed. If the value exceeds the
    /// maximum, it is clamped to the upper bound.
    #[inline]
    pub fn record(&mut self, v: u64) {
        self.rotated_window_mut().saturating_record(v);
        self.sum.add(v);
        self.count.incr();
    }

    /// Record values in the current histogram.
    ///
    /// The current histogram is updated as needed. Values that exceed the
    /// maximum are clamped to the upper bound.
    #[inline]
    pub fn record_n(&mut self, v: u64, n: usize) {
        self.rotated_window_mut().saturating_record_n(v, n as u64);
        self.sum.add(v * n as u64);
        self.count.add(n as u64);
    }

    /// Get a mutable reference to the current histogram, rotating windows as
    /// necessary.
    #[inline]
    fn rotated_window_mut(&mut self) -> &mut Histogram<u64> {
        let now = time::Instant::now();
        if now >= self.next_rotate {
            // Advance windows per elapsed time. If the
            let rotations =
                ((now - self.next_rotate).as_millis() / self.rotate_interval.as_millis()) + 1;
            for _ in 0..rotations {
                self.window_idx = (self.window_idx + 1) % self.windows.len();
                self.windows[self.window_idx].reset();
            }
            self.next_rotate = now + self.rotate_interval;
        }

        &mut self.windows[self.window_idx]
    }

    /// Lock the inner report, clear it, and repopulate from the active windows.
    fn lock_report(&self) -> MutexGuard<'_, Histogram<u64>> {
        let mut report = self.report.lock();
        // Remove all values from the merged
        report.reset();
        for w in self.windows.iter() {
            report
                .add(w)
                .expect("Histograms must share a maximum value");
        }
        report
    }
}

impl<F: Factor> FmtMetric for Summary<F> {
    const KIND: &'static str = "summary";

    fn fmt_metric<N: fmt::Display>(&self, f: &mut fmt::Formatter<'_>, name: N) -> fmt::Result {
        {
            let report = self.lock_report();
            for q in self.quantiles.iter() {
                let v = Counter::<F>::from(report.value_at_quantile(*q));
                v.fmt_metric_labeled(f, &name, FmtQuantile(q))?;
            }
        }
        self.count.fmt_metric(f, format_args!("{}_count", name))?;
        self.sum.fmt_metric(f, format_args!("{}_sum", name))?;
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
        {
            let report = self.lock_report();
            for q in self.quantiles.iter() {
                let v = Counter::<F>::from(report.value_at_quantile(*q));
                v.fmt_metric_labeled(f, &name, (FmtQuantile(q), &labels))?;
            }
        }
        self.count
            .fmt_metric_labeled(f, format_args!("{}_count", name), &labels)?;
        self.sum
            .fmt_metric_labeled(f, format_args!("{}_sum", name), &labels)?;
        Ok(())
    }
}

// === impl FmtQuantile ===

impl<'q> FmtLabels for FmtQuantile<'q> {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "quantile=\"{}\"", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FmtMetrics, MillisAsSeconds};
    use tokio::time;

    crate::metrics! {
        basic: Summary { "basic summary "},
        scaled: Summary<MillisAsSeconds> { "scaled summary "}
    }

    struct Fmt {
        basic: Summary,
        scaled: Summary<MillisAsSeconds>,
    }

    impl FmtMetrics for Fmt {
        fn fmt_metrics(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            basic.fmt_metric(f, &self.basic)?;
            scaled.fmt_metric(f, &self.scaled)?;
            Ok(())
        }
    }

    #[test]
    fn fmt() {
        let output = {
            let mut f = Fmt {
                basic: Summary::new(2, time::Duration::from_secs(2), 100000, 5).unwrap(),
                scaled: Summary::new(2, time::Duration::from_secs(2), 100000, 5).unwrap(),
            };

            record(&mut f.basic);
            record(&mut f.scaled);
            f.as_display()
                .to_string()
                .lines()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        };

        for (output, expected) in output.iter().zip(EXPECTED) {
            assert_eq!(output, expected);
        }
        assert_eq!(output.len(), EXPECTED.len());

        fn record<F>(s: &mut Summary<F>) {
            s.record_n(1, 2500);
            s.record_n(2, 2500);
            s.record_n(10, 2500);
            s.record_n(100, 1500);
            s.record_n(1000, 900);
            s.record_n(10000, 90);
            s.record_n(100000, 9);
            s.record(100001)
        }

        const EXPECTED: &'static [&'static str] = &[
            "basic{quantile=\"0\"} 1",
            "basic{quantile=\"0.5\"} 2",
            "basic{quantile=\"0.75\"} 10",
            "basic{quantile=\"0.9\"} 100",
            "basic{quantile=\"0.99\"} 1000",
            "basic{quantile=\"0.999\"} 10000",
            "basic{quantile=\"1\"} 100001",
            "basic_count 10000",
            "basic_sum 2982501",
            "scaled{quantile=\"0\"} 0.001",
            "scaled{quantile=\"0.5\"} 0.002",
            "scaled{quantile=\"0.75\"} 0.01",
            "scaled{quantile=\"0.9\"} 0.1",
            "scaled{quantile=\"0.99\"} 1",
            "scaled{quantile=\"0.999\"} 10",
            "scaled{quantile=\"1\"} 100.001",
            "scaled_count 10000",
            "scaled_sum 2982.501",
        ];
    }

    #[tokio::test]
    async fn windows() {
        time::pause();

        const WINDOWS: u32 = 2;
        const ROTATE_INTERVAL: time::Duration = time::Duration::from_secs(10);
        let mut s = Summary::<()>::new(WINDOWS, WINDOWS * ROTATE_INTERVAL, 100000, 5).unwrap();

        s.record_n(1, 5_000);
        s.record_n(2, 5_000);
        {
            let h = s.lock_report();
            assert_eq!(h.value_at_quantile(0.5), 1);
            assert_eq!(h.len(), 10000);
        }
        assert_eq!(s.count.value(), 10000.0);
        assert_eq!(s.sum.value(), 15_000.0);

        time::advance(ROTATE_INTERVAL).await;

        s.record_n(1, 4999);
        s.record_n(3, 5001);
        {
            let h = s.lock_report();
            assert_eq!(h.value_at_quantile(0.5), 2);
            assert_eq!(h.len(), 20000);
        }
        assert_eq!(s.count.value(), 20000.0);
        assert_eq!(s.sum.value(), 35_002.0);

        time::advance(ROTATE_INTERVAL).await;

        s.record_n(4, 10_000);
        {
            let h = s.lock_report();
            assert_eq!(h.value_at_quantile(0.5), 3);
            assert_eq!(h.len(), 20000);
        }
        assert_eq!(s.count.value(), 30000.0);
        assert_eq!(s.sum.value(), 75_002.0);

        time::advance(2 * ROTATE_INTERVAL).await;

        s.record_n(1, 10_000);
        {
            let h = s.lock_report();
            assert_eq!(h.value_at_quantile(0.5), 1);
            assert_eq!(h.len(), 10000);
        }
        assert_eq!(s.count.value(), 40000.0);
        assert_eq!(s.sum.value(), 85_002.0);
    }
}
