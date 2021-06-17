use linkerd_error_metrics::{FmtLabels, LabelError};
use std::fmt;

#[derive(Eq, PartialEq, Debug, Copy, Clone)]
pub struct WithDetectTimeout<L>(L);

pub type ErrorRegistry = linkerd_error_metrics::Registry<ErrorLabels>;

#[derive(Eq, PartialEq, Debug, Clone)]
pub enum ErrorLabels {
    Timeout,
    // Inner(L),
}

impl FmtLabels for ErrorLabels /*<L>*/ {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => f.pad("timeout"),
            // Self::Inner(ref inner) => inner.fmt_labels(f),
        }
    }
}
