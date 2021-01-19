pub use crate::metrics::{Direction, OutboundEndpointLabels};
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
    OutboundConnect(OutboundEndpointLabels),
    InboundConnect,
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
}

impl FmtLabels for Key {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Accept(direction, identity) => {
                direction.fmt_labels(f)?;
                write!(f, ",peer=\"src\",")?;
                TlsAccept::from(identity).fmt_labels(f)
            }
            Self::OutboundConnect(endpoint) => {
                Direction::Out.fmt_labels(f)?;
                write!(f, ",peer=\"dst\",")?;
                endpoint.fmt_labels(f)
            }
            Self::InboundConnect => {
                const NO_TLS: tls::client::ConditionalServerId =
                    Conditional::None(tls::client::NoServerId::Loopback);

                Direction::In.fmt_labels(f)?;
                write!(f, ",peer=\"dst\",")?;
                TlsConnect(&NO_TLS).fmt_labels(f)
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
