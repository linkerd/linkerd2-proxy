use std::fmt;
use std::marker::{PhantomData, Sized};

/// Writes a block of metrics in prometheus-formatted output.
pub trait FmtMetrics {
    fn fmt_metrics(&self, f: &mut fmt::Formatter) -> fmt::Result;

    fn as_display(&self) -> DisplayMetrics<&Self> where Self: Sized {
        DisplayMetrics(self)
    }
}

/// Adapts `FmtMetrics` to `fmt::Display`.
pub struct DisplayMetrics<F>(F);

impl<F: FmtMetrics> fmt::Display for DisplayMetrics<F> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt_metrics(f)
    }
}

/// Writes a series of key-quoted-val pairs for use as prometheus labels.
pub trait FmtLabels {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result;
}

/// Writes a metric in prometheus-formatted output.
///
/// This trait is implemented by `Counter`, `Gauge`, and `Histogram` to account for the
/// differences in formatting each type of metric. Specifically, `Histogram` formats a
/// counter for each bucket, as well as a count and total sum.
pub trait FmtMetric {
    /// The metric's `TYPE` in help messages.
    const KIND: &'static str;

    /// Writes a metric with the given name and no labels.
    fn fmt_metric<N: fmt::Display>(&self, f: &mut fmt::Formatter, name: N) -> fmt::Result;

    /// Writes a metric with the given name and labels.
    fn fmt_metric_labeled<N, L>(&self, f: &mut fmt::Formatter, name: N, labels: L) -> fmt::Result
    where
        N: fmt::Display,
        L: FmtLabels;
}

/// Describes a metric statically.
///
/// Formats help messages and metric values for prometheus output.
pub struct Metric<'a, M: FmtMetric> {
    pub name: &'a str,
    pub help: &'a str,
    pub _p: PhantomData<M>,
}

// ===== impl Metric =====

impl<'a, M: FmtMetric> Metric<'a, M> {
    /// Formats help messages for this metric.
    pub fn fmt_help(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(f, "# HELP {} {}", self.name, self.help)?;
        writeln!(f, "# TYPE {} {}", self.name, M::KIND)?;
        Ok(())
    }

    /// Formats a single metric without labels.
    pub fn fmt_metric(&self, f: &mut fmt::Formatter, metric: M) -> fmt::Result {
        metric.fmt_metric(f, self.name)
    }

    /// Formats a single metric across labeled scopes.
    pub fn fmt_scopes<'s, L, S: 's, I, F>(
        &self,
        f: &mut fmt::Formatter,
        scopes: I,
        to_metric: F
    ) -> fmt::Result
    where
        L: FmtLabels,
        I: IntoIterator<Item = (L, &'s S)>,
        F: Fn(&S) -> &M
    {
        for (labels, scope) in scopes {
            to_metric(scope).fmt_metric_labeled(f, self.name, labels)?;
        }

        Ok(())
    }
}

// ===== impl FmtLabels =====

impl<'a, A: FmtLabels + 'a> FmtLabels for &'a A {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        (*self).fmt_labels(f)
    }
}

impl<A: FmtLabels, B: FmtLabels> FmtLabels for (A, B) {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt_labels(f)?;
        f.pad(",")?;
        self.1.fmt_labels(f)?;

        Ok(())
    }
}

impl<A: FmtLabels, B: FmtLabels> FmtLabels for (A, Option<B>) {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt_labels(f)?;
        if let Some(ref b) = self.1 {
            f.pad(",")?;
            b.fmt_labels(f)?;
        }

        Ok(())
    }
}

impl<A: FmtLabels, B: FmtLabels> FmtLabels for (Option<A>, B) {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if let Some(ref a) = self.0 {
            a.fmt_labels(f)?;
            f.pad(",")?;
        }
        self.1.fmt_labels(f)?;

        Ok(())
    }
}

// ===== impl FmtMetrics =====

impl<'a, A: FmtMetrics + 'a> FmtMetrics for &'a A {
    fn fmt_metrics(&self, f: &mut fmt::Formatter) -> fmt::Result {
        (*self).fmt_metrics(f)
    }
}

