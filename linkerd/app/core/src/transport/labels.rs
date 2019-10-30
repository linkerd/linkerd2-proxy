use super::tls;
use linkerd2_conditional::Conditional;
use linkerd2_metrics::FmtLabels;
use linkerd2_identity as identity;
use std::fmt;
use std::net::{IpAddr, SocketAddr};

/// Describes a class of transport.
///
/// A `Metrics` type exists for each unique `Key`.
///
/// Implements `FmtLabels`.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Key {
    direction: Direction,
    addrs: Addrs,
    identity: Identity,
    protocol: Protocol,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
enum Direction {
    Outbound,
    Inbound,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
struct Addrs {
    src_addr: Option<IpAddr>,
    dst_addr: SocketAddr,
}  

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct TlsStatus(tls::Conditional<identity::Name>);

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
struct Identity {
    remote: TlsStatus,
    local: tls::Conditional<identity::Name>,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
enum Protocol {
    TCP,
    HTTP,
}

// ===== impl Key =====

impl Key {
    pub fn accept(
        src_addr: SocketAddr,
        dst_addr: SocketAddr,
        remote_identity: tls::Conditional<&identity::Name>,
        local_identity: tls::Conditional<&identity::Name>,
        http: bool
    ) -> Self {
        let protocol = if http {
            Protocol::HTTP
        } else {
            Protocol::TCP
        };
        Self {
            direction: Direction::Inbound,
            addrs: Addrs {
                src_addr: Some(src_addr.ip()),
                dst_addr,
            },
            identity: Identity {
                remote: TlsStatus(remote_identity.cloned()),
                local: local_identity.cloned(),
            },
            protocol,
        }
    }

    pub fn connect(
        dst_addr: SocketAddr,
        local_identity: tls::Conditional<&identity::Name>,
        remote_identity: tls::Conditional<&identity::Name>,
        http: bool
    ) -> Self {
        let protocol = if http {
            Protocol::HTTP
        } else {
            Protocol::TCP
        };
        Self {
            direction: Direction::Outbound,
            addrs: Addrs {
                src_addr: None,
                dst_addr,
            },
            identity: Identity {
                remote: TlsStatus(remote_identity.cloned()),
                local: local_identity.cloned(),
            },
            protocol,
        }
    }
}

impl FmtLabels for Key {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (((&self.direction, &self.addrs), &self.identity), &self.protocol).fmt_labels(f)
    }
}

impl FmtLabels for Direction {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Direction::Inbound => write!(f, "direction=\"inbound\""),
            Direction::Outbound => write!(f, "direction=\"outbound\""),
        }
    }
}

impl FmtLabels for Addrs {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(src) = &self.src_addr {
            write!(f, "src_addr=\"{}\",", src)?;
        }
        write!(f, "dst_addr=\"{}\"", self.dst_addr)
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

impl FmtLabels for Identity {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.remote.fmt_labels(f)?;
        // Only include the local identity labels if the connection has TLS.
        if self.remote.0.is_some() {
            match &self.local {
                Conditional::None(_) => Ok(()),
                Conditional::Some(name) => write!(f, ",local_identity=\"{}\"", name),
            }
        } else {
            Ok(())
        }
    }
}
