pub use crate::metrics::{Direction, EndpointLabels, TlsId};
use linkerd_conditional::Conditional;
use linkerd_identity as id;
use linkerd_metrics::FmtLabels;
use linkerd_tls as tls;
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

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct TlsStatus(tls::Conditional<TlsId>);

// === impl Key ===

impl Key {
    pub fn accept(direction: Direction, id: tls::Conditional<tls::ClientId>) -> Self {
        Self::Accept(direction, id.into())
    }
}

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

impl TlsStatus {
    pub const LOOPBACK: Self = Self(Conditional::None(tls::ReasonForNoPeerName::Loopback));
}

impl From<tls::Conditional<tls::ClientId>> for TlsStatus {
    fn from(id: tls::Conditional<tls::ClientId>) -> Self {
        Self(id.map(TlsId::ClientId))
    }
}

impl From<tls::Conditional<tls::ServerId>> for TlsStatus {
    fn from(id: tls::Conditional<tls::ServerId>) -> Self {
        Self(id.map(TlsId::ServerId))
    }
}

impl From<tls::Conditional<TlsId>> for TlsStatus {
    fn from(inner: tls::Conditional<TlsId>) -> Self {
        TlsStatus(inner)
    }
}

impl Into<tls::Conditional<id::Name>> for TlsStatus {
    fn into(self) -> tls::Conditional<id::Name> {
        self.0.map(|id| match id {
            TlsId::ClientId(tls::ClientId(id)) => id,
            TlsId::ServerId(tls::ServerId(id)) => id,
        })
    }
}

impl fmt::Display for TlsStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Conditional::Some(_) => write!(f, "true"),
            Conditional::None(ref r) => fmt::Display::fmt(&r, f),
        }
    }
}

impl FmtLabels for TlsStatus {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Conditional::None(tls::ReasonForNoPeerName::LocalIdentityDisabled) => {
                write!(f, "tls=\"disabled\"")
            }
            Conditional::None(ref why) => {
                write!(f, "tls=\"no_identity\",no_tls_reason=\"{}\"", why)
            }
            Conditional::Some(ref id) => {
                write!(f, "tls=\"true\",")?;
                id.fmt_labels(f)
            }
        }
    }
}
