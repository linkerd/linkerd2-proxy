use linkerd_error_metrics::{label, FmtLabels, LabelError};
use std::{error::Error, fmt, marker::PhantomData};

#[derive(Eq, PartialEq, Debug, Copy, Clone)]
pub struct LabelTimeout<P, L = label::Other> {
    inner: L,
    _proto: PhantomData<fn(P)>,
}

pub type ErrorRegistry<L> = linkerd_error_metrics::Registry<ErrorLabels<L>>;

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

impl<P> LabelTimeout<P>
where
    P: 'static,
{
    pub fn new() -> Self {
        Self::default()
    }

    pub fn or_else<L>(self, inner: L) -> LabelTimeout<P, L> {
        LabelTimeout {
            inner,
            _proto: self._proto,
        }
    }
}

impl<P> Default for LabelTimeout<P> {
    fn default() -> Self {
        Self {
            inner: label::Other::default(),
            _proto: PhantomData,
        }
    }
}

impl<P, E, L> LabelError<E> for LabelTimeout<P, L>
where
    P: 'static,
    E: Error + 'static,
    L: LabelError<E>,
{
    type Labels = ErrorLabels<L::Labels>;

    fn label_error(&self, err: &E) -> Self::Labels {
        let mut curr: Option<&dyn Error> = Some(err);
        while let Some(err) = curr {
            if err.is::<crate::DetectTimeout<P>>() {
                return ErrorLabels::Timeout;
            }
            curr = err.source();
        }

        ErrorLabels::Inner(self.inner.label_error(err))
    }
}
