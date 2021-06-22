use linkerd_error::Error;
use linkerd_error_metrics::{label, FmtLabels, LabelError};
use linkerd_tls::server::DetectTimeout;
use std::fmt;

pub type ErrorRegistry = linkerd_error_metrics::Registry<ErrorLabels<label::IoLabels>>;

#[derive(Eq, PartialEq, Debug, Copy, Clone, Default)]
pub struct LabelTimeout<L = label::Io> {
    inner: L,
}

#[derive(Eq, PartialEq, Debug, Clone, Hash)]
pub enum ErrorLabels<L> {
    Timeout,
    Inner(L),
}

impl<L: FmtLabels> FmtLabels for ErrorLabels<L> {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => f.pad("error=\"timeout\""),
            Self::Inner(ref inner) => inner.fmt_labels(f),
        }
    }
}

impl LabelTimeout {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn or_else<L>(self, inner: L) -> LabelTimeout<L> {
        LabelTimeout { inner }
    }
}

impl<L> LabelError<Error> for LabelTimeout<L>
where
    L: LabelError<Error>,
{
    type Labels = ErrorLabels<L::Labels>;

    fn label_error(&self, err: &Error) -> Self::Labels {
        let mut curr: Option<&dyn std::error::Error> = Some(err.as_ref());
        while let Some(err) = curr {
            if err.is::<DetectTimeout>() {
                return ErrorLabels::Timeout;
            }
            curr = err.source();
        }

        ErrorLabels::Inner(self.inner.label_error(err))
    }
}
