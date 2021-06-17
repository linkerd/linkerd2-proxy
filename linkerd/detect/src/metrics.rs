use linkerd_error_metrics::{FmtLabels, LabelError};
use std::fmt;

#[derive(Eq, PartialEq, Debug, Copy, Clone)]
pub struct LabelTimeout;

pub type ErrorRegistry = linkerd_error_metrics::Registry<ErrorLabels>;

#[derive(Eq, PartialEq, Debug, Clone, Hash)]
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

impl<P> LabelError<crate::DetectTimeout<P>> for LabelTimeout {
    type Labels = ErrorLabels;

    fn label_error(&self, _: &crate::DetectTimeout<P>) -> Self::Labels {
        ErrorLabels::Timeout
    }
}
