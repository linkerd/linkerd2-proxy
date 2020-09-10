use super::tls;
pub use crate::metric_labels::{Direction, EndpointLabels};
use linkerd2_conditional::Conditional;
use linkerd2_metrics::FmtLabels;
use std::fmt;

/// Describes a class of transport.
///
/// A `Metrics` type exists for each unique `Key`.
///
/// Implements `FmtLabels`.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum Key {
    Accept(Direction, TlsStatus),
    Connect(EndpointLabels),
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct TlsStatus(tls::Conditional<()>);

// === impl Key ===

impl FmtLabels for Key {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Accept(direction, identity) => {
                write!(f, "peer=\"src\",")?;
                (direction, identity).fmt_labels(f)
            }
            Self::Connect(labels) => {
                write!(f, "peer=\"dst\",")?;
                labels.fmt_labels(f)
            }
        }
    }
}

// === impl TlsStatus ===

impl<T> From<tls::Conditional<T>> for TlsStatus {
    fn from(inner: tls::Conditional<T>) -> Self {
        TlsStatus(inner.map(|_| ()))
    }
}

impl Into<tls::Conditional<()>> for TlsStatus {
    fn into(self) -> tls::Conditional<()> {
        self.0
    }
}

impl fmt::Display for TlsStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Conditional::Some(()) => write!(f, "true"),
            Conditional::None(r) => fmt::Display::fmt(&r, f),
        }
    }
}

impl FmtLabels for TlsStatus {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Self(Conditional::None(tls::ReasonForNoPeerName::LocalIdentityDisabled)) = self {
            return write!(f, "tls=\"disabled\"");
        }
        if let Self(Conditional::None(why)) = self {
            return write!(f, "tls=\"no_identity\",no_tls_reason=\"{}\"", why);
        }

        write!(f, "tls=\"{}\"", self)
    }
}
