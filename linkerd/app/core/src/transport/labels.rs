use super::tls;
use linkerd2_conditional::Conditional;
use linkerd2_metrics::FmtLabels;
use linkerd2_identity as identity;
use std::fmt;
use std::net::SocketAddr;

/// Describes a class of transport.
///
/// A `Metrics` type exists for each unique `Key`.
///
/// Implements `FmtLabels`.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Key {
    direction: Direction,
    remote_addr: RemoteAddr,
    tls_status: TlsStatus,
    protocol: Protocol,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
struct Direction(&'static str);

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
struct RemoteAddr(SocketAddr);

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct TlsStatus(tls::Conditional<identity::Name>);

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
enum Protocol {
    TCP,
    HTTP,
}

// ===== impl Key =====

impl Key {
    pub fn accept(remote_addr: SocketAddr, tls: &tls::Conditional<identity::Name>, http: bool) -> Self {
        let protocol = if http {
            Protocol::HTTP
        } else {
            Protocol::TCP
        };
        Self {
            direction: Direction("inbound"),
            remote_addr: RemoteAddr(remote_addr),
            tls_status: TlsStatus(tls.clone()),
            protocol,
        }
    }

    pub fn connect(remote_addr: SocketAddr, tls: &tls::Conditional<identity::Name>, http: bool) -> Self {
        let protocol = if http {
            Protocol::HTTP
        } else {
            Protocol::TCP
        };
        Self {
            direction: Direction("outbound"),
             remote_addr: RemoteAddr(remote_addr),
            tls_status: TlsStatus(tls.clone()),
            protocol,
        }
    }
}

impl FmtLabels for Key {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (((&self.direction, &self.remote_addr), &self.tls_status), &self.protocol).fmt_labels(f)
    }
}

impl FmtLabels for Direction {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "direction=\"{}\"", self.0)
    }
}

impl FmtLabels for RemoteAddr {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "remote_addr=\"{}\"", self.0)
    }
}

impl FmtLabels for Protocol {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Protocol::TCP => write!(f, "protocol=\"tcp\""),
            Protocol::HTTP => write!(f, "protocol=\"http\""),
        }
    }
}

impl FmtLabels for TlsStatus {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Conditional::None(reason) => write!(f, "tls=\"false\",no_tls_reason=\"{}\"", reason),
            Conditional::Some(name) => write!(f, "tls=\"true\",remote_identity=\"{}\"", name),
        }
    }
}

impl fmt::Display for TlsStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Conditional::Some(_) => write!(f, "true"),
            Conditional::None(r) => fmt::Display::fmt(&r, f),
        }
    }
}

impl From<tls::Conditional<&identity::Name>> for TlsStatus {
    fn from(tls: tls::Conditional<&identity::Name>) -> TlsStatus {
        TlsStatus(tls.cloned())
    }
}
