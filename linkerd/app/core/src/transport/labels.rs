pub use crate::metrics::{Direction, OutboundEndpointLabels};
use linkerd_conditional::Conditional;
use linkerd_metrics::FmtLabels;
use linkerd_tls as tls;
use std::{fmt, net::SocketAddr};

/// Describes a class of transport.
///
/// A `Metrics` type exists for each unique `Key`.
///
/// Implements `FmtLabels`.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum Key {
    Accept {
        direction: Direction,
        tls: tls::ConditionalServerTls,
        target_addr: SocketAddr,
    },
    OutboundConnect(OutboundEndpointLabels),
    InboundConnect,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) struct TlsAccept<'t>(&'t tls::ConditionalServerTls);

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) struct TlsConnect<'t>(&'t tls::ConditionalClientTls);

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) struct TargetAddr(pub(crate) SocketAddr);

// === impl Key ===

impl Key {
    pub fn accept(
        direction: Direction,
        tls: tls::ConditionalServerTls,
        target_addr: SocketAddr,
    ) -> Self {
        Self::Accept {
            direction,
            tls,
            target_addr,
        }
    }
}

impl FmtLabels for Key {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Accept {
                direction,
                tls,
                target_addr,
            } => {
                direction.fmt_labels(f)?;
                f.write_str(",peer=\"src\",")?;
                (TargetAddr(*target_addr), TlsAccept::from(tls)).fmt_labels(f)
            }
            Self::OutboundConnect(endpoint) => {
                Direction::Out.fmt_labels(f)?;
                write!(f, ",peer=\"dst\",")?;
                endpoint.fmt_labels(f)
            }
            Self::InboundConnect => {
                const NO_TLS: tls::client::ConditionalClientTls =
                    Conditional::None(tls::NoClientTls::Loopback);

                Direction::In.fmt_labels(f)?;
                write!(f, ",peer=\"dst\",")?;
                TlsConnect(&NO_TLS).fmt_labels(f)
            }
        }
    }
}

// === impl TlsAccept ===

impl<'t> From<&'t tls::ConditionalServerTls> for TlsAccept<'t> {
    fn from(c: &'t tls::ConditionalServerTls) -> Self {
        TlsAccept(c)
    }
}

impl<'t> FmtLabels for TlsAccept<'t> {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Conditional::None(tls::NoServerTls::Disabled) => {
                write!(f, "tls=\"disabled\"")
            }
            Conditional::None(why) => {
                write!(f, "tls=\"no_identity\",no_tls_reason=\"{}\"", why)
            }
            Conditional::Some(tls::ServerTls::Established { client_id, .. }) => match client_id {
                Some(id) => write!(f, "tls=\"true\",client_id=\"{}\"", id),
                None => write!(f, "tls=\"true\",client_id=\"\""),
            },
            Conditional::Some(tls::ServerTls::Passthru { sni }) => {
                write!(f, "tls=\"opaque\",sni=\"{}\"", sni)
            }
        }
    }
}

// === impl TlsConnect ===

impl<'t> From<&'t tls::ConditionalClientTls> for TlsConnect<'t> {
    fn from(s: &'t tls::ConditionalClientTls) -> Self {
        TlsConnect(s)
    }
}

impl<'t> FmtLabels for TlsConnect<'t> {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Conditional::None(tls::NoClientTls::Disabled) => {
                write!(f, "tls=\"disabled\"")
            }
            Conditional::None(why) => {
                write!(f, "tls=\"no_identity\",no_tls_reason=\"{}\"", why)
            }
            Conditional::Some(tls::ClientTls { server_id, .. }) => {
                write!(f, "tls=\"true\",server_id=\"{}\"", server_id)
            }
        }
    }
}

// === impl TargetAddr ===

impl FmtLabels for TargetAddr {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "target_addr=\"{}\"", self.0)
    }
}
