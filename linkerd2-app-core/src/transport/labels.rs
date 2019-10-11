use super::tls;
use linkerd2_conditional::Conditional;
use linkerd2_metrics::FmtLabels;
use std::fmt;

/// Describes a class of transport.
///
/// A `Metrics` type exists for each unique `Key`.
///
/// Implements `FmtLabels`.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct Key {
    direction: Direction,
    peer: Peer,
    tls_status: TlsStatus,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
struct Direction(&'static str);

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
enum Peer {
    /// Represents the side of the proxy that accepts connections.
    Src,
    /// Represents the side of the proxy that opens connections.
    Dst,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct TlsStatus(tls::Conditional<()>);

// ===== impl Key =====

impl Key {
    pub fn accept<T>(direction: &'static str, tls: tls::Conditional<T>) -> Self {
        Self {
            direction: Direction(direction),
            tls_status: TlsStatus(tls.map(|_| ())),
            peer: Peer::Src,
        }
    }

    pub fn connect<T>(direction: &'static str, tls: tls::Conditional<T>) -> Self {
        Self {
            direction: Direction(direction),
            tls_status: TlsStatus(tls.map(|_| ())),
            peer: Peer::Dst,
        }
    }
}

impl FmtLabels for Key {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        ((self.direction, self.peer), self.tls_status).fmt_labels(f)
    }
}

// ===== impl Peer =====

impl FmtLabels for Direction {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "direction=\"{}\"", self.0)
    }
}

// ===== impl Peer =====

impl FmtLabels for Peer {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Peer::Src => f.pad("peer=\"src\""),
            Peer::Dst => f.pad("peer=\"dst\""),
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

impl TlsStatus {
    pub fn no_tls_reason(&self) -> Option<tls::ReasonForNoIdentity> {
        self.0.reason()
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
        if let Some(tls::ReasonForNoIdentity::NoPeerName(why)) = self.no_tls_reason() {
            return write!(f, "tls=\"no_identity\",no_tls_reason=\"{}\"", why);
        }

        write!(f, "tls=\"{}\"", self)
    }
}
