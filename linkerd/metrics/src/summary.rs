// This module is inspired by hdrhistogram-go, which is distributed under the
// MIT license. Copyright (c) 2014 Coda Hale

use crate::{Counter, Factor, FmtLabels, FmtMetric};
pub use hdrhistogram::{AdditionError, CreationError, Histogram, RecordError};
use parking_lot::{MappedMutexGuard, Mutex, MutexGuard};
use std::fmt;
use tokio::time;

/// Summarizes a distribution of values at fixed quantiles over a sliding window.
#[derive(Debug)]
pub struct Summary<F = ()> {
    rotate_interval: time::Duration,

    windows: Mutex<Windows>,

    /// Instead of allocating a new histogram each time a report is formatted, we
    /// hold a single report and reset/repopulate it from the active windows.
    report: Mutex<Histogram<u64>>,
    quantiles: Box<[f64]>,

    /// Count is tracked independently of the histogams so that rotated values
    /// are included.
    count: Counter,
    sum: Counter<F>,
}

#[derive(Debug)]
struct Windows {
    /// A ring buffer of active histograms window. Every `rotate_interval` the
    /// index is advanced and the oldest histogram is reset and reused for new
    /// values. `window_idx` always refers to the index of the newest active
    /// histogram and `window_idx + 1 % windows.len()` is the oldest active
    /// histogram.
    active: Vec<Histogram<u64>>,
    idx: usize,
    next_rotate: time::Instant,
}

/// Helper that lazily formats `quantile` labels.
struct FmtQuantile<'q>(&'q f64);

// === impl Summary ===

impl<F> Summary<F> {
    const DEFAULT_QUANTILES: [f64; 7] = [0.0, 0.50, 0.75, 0.90, 0.99, 0.999, 1.0];

    /// Creates a new summary with the specified number of windows. Values are
    /// are included for at most `lifetime`.
    ///
    /// Histograms are automatically resized to accomodate values,
    pub fn new_resizable(
        n_windows: u32,
        lifetime: time::Duration,
        sigfig: u8,
    ) -> Result<Self, CreationError> {
        let h = Histogram::new(sigfig)?;
        Ok(Self::new_inner(n_windows, lifetime, h))
    }

    /// Creates a new summary with the specified number of windows. Values are
    /// are included for at most `lifetime`.
    ///
    /// Histograms are **not** automatically resized to accomodate values,
    pub fn new_with_max(
        n_windows: u32,
        lifetime: time::Duration,
        high: u64,
        sigfig: u8,
    ) -> Result<Self, CreationError> {
        let h = Histogram::new_with_max(high, sigfig)?;
        Ok(Self::new_inner(n_windows, lifetime, h))
    }

    fn new_inner(n_windows: u32, lifetime: time::Duration, histogram: Histogram<u64>) -> Self {
        debug_assert!(n_windows > 0);
        let rotate_interval = lifetime / n_windows;

        let windows = {
            let mut active = Vec::with_capacity(n_windows as usize);
            for _ in 0..n_windows {
                active.push(histogram.clone());
            }
            let next_rotate = time::Instant::now() + rotate_interval;
            Mutex::new(Windows {
                active,
                idx: 0,
                next_rotate,
            })
        };

        let report = Mutex::new(histogram);

        Self {
            windows,
            rotate_interval,

            report,
            quantiles: Box::new(Self::DEFAULT_QUANTILES),

            count: Counter::new(),
            sum: Counter::new(),
        }
    }

    /// Overrides the default quantiles to be reported.
    pub fn with_quantiles(mut self, qs: impl IntoIterator<Item = f64>) -> Self {
        self.quantiles = qs.into_iter().collect();
        self
    }

    /// Record a value in the current histogram.
    ///
    /// Histograms are rotated as needed.
    #[inline]
    pub fn record(&self, v: u64) -> Result<(), RecordError> {
        self.record_n(v, 1)
    }

    /// Record values in the current histogram.
    ///
    /// Histograms are rotated as needed.
    #[inline]
    pub fn record_n(&self, v: u64, n: usize) -> Result<(), RecordError> {
        self.rotated_window_mut().record_n(v, n as u64)?;
        self.sum.add(v * n as u64);
        self.count.add(n as u64);
        Ok(())
    }

    /// Record a value in the current histogram.
    ///
    /// Histograms are rotated as needed. If the value exceeds the maximum, it is
    /// clamped to the upper bound. This should not be used with resizable
    /// summaries.
    #[inline]
    pub fn saturating_record(&self, v: u64) {
        self.saturating_record_n(v, 1);
    }

    /// Record values in the current histogram.
    ///
    /// Histograms are rotated as needed. Values that exceed the maximum are
    /// clamped to the upper bound. This should not be used with resizable
    /// summaries.
    #[inline]
    pub fn saturating_record_n(&self, v: u64, n: usize) {
        self.rotated_window_mut().saturating_record_n(v, n as u64);
        self.sum.add(v * n as u64);
        self.count.add(n as u64);
    }

    /// Get a mutable reference to the current histogram, rotating windows as
    /// necessary.
    #[inline]
    fn rotated_window_mut(&self) -> MappedMutexGuard<'_, Histogram<u64>> {
        let mut w = self.windows.lock();
        let now = time::Instant::now();
        if now >= w.next_rotate {
            // Advance windows per elapsed time. If the more than one interval
            // has elapsed, clear out intermediate windows so reports only
            // include values in the max lifetime.
            let rotations =
                ((now - w.next_rotate).as_millis() / self.rotate_interval.as_millis()) + 1;
            for _ in 0..rotations {
                let i = (w.idx + 1) % w.active.len();
                w.active[i].reset();
                w.idx = i;
            }
            w.next_rotate = now + self.rotate_interval;
        }

        MutexGuard::map(w, |w| &mut w.active[w.idx])
    }

    /// Lock the inner report, clear it, and repopulate from the active windows.
    fn lock_report(&self) -> MutexGuard<'_, Histogram<u64>> {
        let mut report = self.report.lock();
        // Remove all values from the merged
        report.reset();
        let windows = self.windows.lock();
        for w in windows.active.iter() {
            // The report histogram must have been created with the same
            // configuration as the other histograms, so they all either share a
            // max value or are all resizable.
            report.add(w).expect("Histograms must merge");
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
        basic: Summary { "A simple summary" },
        scaled: Summary<MillisAsSeconds> { "A summary of millis as seconds" }
    }

    struct Fmt {
        basic: Summary,
        scaled: Summary<MillisAsSeconds>,
    }

    impl FmtMetrics for Fmt {
        fn fmt_metrics(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            struct Label;
            impl FmtLabels for Label {
                fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    write!(f, "k=\"v\"")
                }
            }

            basic.fmt_help(f)?;
            basic.fmt_metric(f, &self.basic)?;
            scaled.fmt_help(f)?;
            scaled.fmt_metric_labeled(f, &self.scaled, &Label)?;
            Ok(())
        }
    }

    #[test]
    fn fmt() {
        let output = {
            let mut f = Fmt {
                basic: Summary::new_resizable(2, time::Duration::from_secs(2), 5).unwrap(),
                scaled: Summary::new_resizable(2, time::Duration::from_secs(2), 5).unwrap(),
            };

            record(&mut f.basic).unwrap();
            record(&mut f.scaled).unwrap();
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

        fn record<F>(s: &mut Summary<F>) -> Result<(), RecordError> {
            s.record_n(1, 2500)?;
            s.record_n(2, 2500)?;
            s.record_n(10, 2500)?;
            s.record_n(100, 1500)?;
            s.record_n(1000, 900)?;
            s.record_n(10000, 90)?;
            s.record_n(100000, 9)?;
            s.record(100001)?;
            Ok(())
        }

        const EXPECTED: &[&str] = &[
            "# HELP basic A simple summary",
            "# TYPE basic summary",
            "basic{quantile=\"0\"} 1",
            "basic{quantile=\"0.5\"} 2",
            "basic{quantile=\"0.75\"} 10",
            "basic{quantile=\"0.9\"} 100",
            "basic{quantile=\"0.99\"} 1000",
            "basic{quantile=\"0.999\"} 10000",
            "basic{quantile=\"1\"} 100001",
            "basic_count 10000",
            "basic_sum 2982501",
            "# HELP scaled A summary of millis as seconds",
            "# TYPE scaled summary",
            "scaled{quantile=\"0\",k=\"v\"} 0.001",
            "scaled{quantile=\"0.5\",k=\"v\"} 0.002",
            "scaled{quantile=\"0.75\",k=\"v\"} 0.01",
            "scaled{quantile=\"0.9\",k=\"v\"} 0.1",
            "scaled{quantile=\"0.99\",k=\"v\"} 1",
            "scaled{quantile=\"0.999\",k=\"v\"} 10",
            "scaled{quantile=\"1\",k=\"v\"} 100.001",
            "scaled_count{k=\"v\"} 10000",
            "scaled_sum{k=\"v\"} 2982.501",
        ];
    }

    #[tokio::test]
    async fn windows() {
        time::pause();

        const WINDOWS: u32 = 2;
        const ROTATE_INTERVAL: time::Duration = time::Duration::from_secs(10);
        let s = Summary::<()>::new_resizable(WINDOWS, WINDOWS * ROTATE_INTERVAL, 5).unwrap();

        s.record_n(1, 5_000).unwrap();
        s.record_n(2, 5_000).unwrap();
        {
            let h = s.lock_report();
            assert_eq!(h.value_at_quantile(0.5), 1);
            assert_eq!(h.len(), 10000);
        }
        assert_eq!(s.count.value(), 10000.0);
        assert_eq!(s.sum.value(), 15_000.0);

        time::sleep(ROTATE_INTERVAL).await;

        s.record_n(1, 4999).unwrap();
        s.record_n(3, 5001).unwrap();
        {
            let h = s.lock_report();
            assert_eq!(h.value_at_quantile(0.5), 2);
            assert_eq!(h.len(), 20000);
        }
        assert_eq!(s.count.value(), 20000.0);
        assert_eq!(s.sum.value(), 35_002.0);

        time::sleep(ROTATE_INTERVAL).await;

        s.record_n(4, 10_000).unwrap();
        {
            let h = s.lock_report();
            assert_eq!(h.value_at_quantile(0.5), 3);
            assert_eq!(h.len(), 20000);
        }
        assert_eq!(s.count.value(), 30000.0);
        assert_eq!(s.sum.value(), 75_002.0);

        time::sleep(2 * ROTATE_INTERVAL).await;

        s.record_n(1, 10_000).unwrap();
        {
            let h = s.lock_report();
            assert_eq!(h.value_at_quantile(0.5), 1);
            assert_eq!(h.len(), 10000);
        }
        assert_eq!(s.count.value(), 40000.0);
        assert_eq!(s.sum.value(), 85_002.0);
    }
}
