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
    addrs: Addrs,
    remote_identity: TlsStatus,
    local_identity: LocalIdentity,
    protocol: Protocol,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
enum Direction {
    Outbound,
    Inbound,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
struct Addrs {
    src_addr: SocketAddr,
    dst_addr: SocketAddr,
}  

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct TlsStatus(tls::Conditional<identity::Name>);

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
struct LocalIdentity(tls::Conditional<identity::Name>);

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
                src_addr,
                dst_addr,
            },
            remote_identity: TlsStatus(remote_identity.cloned()),
            local_identity: LocalIdentity(local_identity.cloned()),
            protocol,
        }
    }

    pub fn connect(
        src_addr: SocketAddr,
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
                src_addr,
                dst_addr,
            },
            remote_identity: TlsStatus(remote_identity.cloned()),
            local_identity: LocalIdentity(local_identity.cloned()),
            protocol,
        }
    }
}

impl FmtLabels for Key {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        ((((&self.direction, &self.addrs), &self.remote_identity), &self.local_identity), &self.protocol).fmt_labels(f)
    }
}

impl FmtLabels for Direction {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Direction::Inbound => write!(f, "direction=\"inbound\","),
            Direction::Outbound => write!(f, "direction=\"outbound\","),
        }
    }
}

impl FmtLabels for Addrs {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "src_addr=\"{}\",", self.src_addr.ip())?;
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

impl FmtLabels for LocalIdentity {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Conditional::None(_) => write!(f, "local_identity=\"\""),
            Conditional::Some(name) => write!(f, "local_identity=\"{}\"", name),
        }
    }
}
