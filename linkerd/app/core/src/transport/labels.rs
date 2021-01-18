pub use crate::metrics::{Direction, EndpointLabels};
use linkerd_conditional::Conditional;
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
    Accept(Direction, tls::server::ConditionalTls),
    Connect(EndpointLabels),
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct TlsAccept<'t>(&'t tls::server::ConditionalTls);

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct TlsConnect<'t>(&'t tls::ConditionalServerId);

// === impl Key ===

impl Key {
    pub fn accept(direction: Direction, id: tls::server::ConditionalTls) -> Self {
        Self::Accept(direction, id)
    }

    pub fn connect(ep: impl Into<EndpointLabels>) -> Self {
        Self::Connect(ep.into())
    }
}

impl FmtLabels for Key {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Accept(direction, identity) => {
                write!(f, "peer=\"src\",")?;
                (direction, TlsAccept::from(identity)).fmt_labels(f)
            }
            Self::Connect(labels) => {
                write!(f, "peer=\"dst\",")?;
                labels.fmt_labels(f)
            }
        }
    }
}

// === impl TlsAccept ===

impl<'t> From<&'t tls::server::ConditionalTls> for TlsAccept<'t> {
    fn from(c: &'t tls::server::ConditionalTls) -> Self {
        TlsAccept(c)
    }
}

impl<'t> FmtLabels for TlsAccept<'t> {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Conditional::None(tls::server::NoTls::Disabled) => {
                write!(f, "tls=\"disabled\"")
            }
            Conditional::None(why) => {
                write!(f, "tls=\"no_identity\",no_tls_reason=\"{}\"", why)
            }
            Conditional::Some(Some(id)) => {
                write!(f, "tls=\"true\",client_id=\"{}\"", id)
            }
            Conditional::Some(None) => {
                write!(f, "tls=\"true\",client_id=\"\"")
            }
        }
    }
}

// === impl TlsConnect ===

impl<'t> From<&'t tls::ConditionalServerId> for TlsConnect<'t> {
    fn from(s: &'t tls::ConditionalServerId) -> Self {
        TlsConnect(s)
    }
}

impl<'t> FmtLabels for TlsConnect<'t> {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Conditional::None(tls::NoServerId::Disabled) => {
                write!(f, "tls=\"disabled\"")
            }
            Conditional::None(why) => {
                write!(f, "tls=\"no_identity\",no_tls_reason=\"{}\"", why)
            }
            Conditional::Some(id) => {
                write!(f, "tls=\"true\",server_id=\"{}\"", id)
            }
        }
    }
}
