use super::LabelError;
use linkerd_errno::Errno;
use linkerd_error::Error;
use linkerd_metrics::FmtLabels;
use std::{fmt, io};

/// Catchall for uncategorized labels.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Default)]
pub struct Other {
    _p: (),
}

#[derive(Copy, Clone, Default)]
pub struct Io<L = Other> {
    inner: L,
}

#[derive(Copy, Clone, Eq, PartialEq, Hash)]

pub enum IoLabels<L = Other> {
    Errno(Errno),
    NoErrno,
    Inner(L),
}

// === impl Other ===

impl FmtLabels for Other {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("error=\"other\"")
    }
}

impl<E> LabelError<E> for Other {
    type Labels = Other;
    fn label_error(&self, _: &E) -> Self::Labels {
        Other::default()
    }
}

// === impl Io ===

impl Io {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn or_else<L>(self, inner: L) -> Io<L> {
        Io { inner }
    }
}

impl<L> LabelError<Error> for Io<L>
where
    L: LabelError<Error>,
{
    type Labels = IoLabels<L::Labels>;
    fn label_error(&self, error: &Error) -> Self::Labels {
        let mut curr: Option<&(dyn std::error::Error + 'static)> = Some(error.as_ref());
        while let Some(err) = curr {
            if let Some(err) = err.downcast_ref::<io::Error>() {
                return match err.raw_os_error() {
                    Some(code) => IoLabels::Errno(code.into()),
                    None => IoLabels::NoErrno,
                };
            }
            curr = err.source();
        }

        IoLabels::Inner(self.inner.label_error(error))
    }
}

// === impl IoLabels ===

impl<L: FmtLabels> FmtLabels for IoLabels<L> {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Errno(errno) => write!(f, "error=\"io\",errno=\"{}\"", errno),
            Self::NoErrno => f.write_str("error=\"io\""),
            Self::Inner(inner) => inner.fmt_labels(f),
        }
    }
}
