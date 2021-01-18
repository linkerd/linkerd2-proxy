#![deny(warnings, rust_2018_idioms)]

use linkerd_identity as id;
pub use linkerd_identity::LocalId;
pub use rustls::TLSError as Error;
use std::fmt;

pub mod accept;
pub mod client;
mod conditional_accept;

pub use self::accept::{ClientId, NewDetectTls};
pub use self::client::{Client, ServerId};

/// Describes whether or not a connection was secured with TLS and, if it was
/// not, the reason why.
pub type Conditional<T> = linkerd_conditional::Conditional<T, ReasonForNoPeerName>;

pub type PeerIdentity = Conditional<id::Name>;

// === imp ReasonForNoPeerName ===

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReasonForNoPeerName {
    /// The destination service didn't give us the identity, which is its way
    /// of telling us that we shouldn't do TLS for this endpoint.
    NotProvidedByServiceDiscovery,

    /// No TLS is wanted because the connection is a loopback connection which
    /// doesn't need or support TLS.
    Loopback,

    // The connection was insecure.
    NoTlsFromRemote,

    // Identity was not provided by the remote peer.
    NoPeerIdFromRemote,

    // TLS termination was not attempted on this port.
    PortSkipped,

    // Discovery is not performed for non-HTTP connections when in "ingress mode".
    IngressNonHttp,

    /// Identity is administratively disabled.
    LocalIdentityDisabled,
}

impl fmt::Display for ReasonForNoPeerName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReasonForNoPeerName::LocalIdentityDisabled => write!(f, "disabled"),
            ReasonForNoPeerName::Loopback => write!(f, "loopback"),
            ReasonForNoPeerName::PortSkipped => write!(f, "port_skipped"),
            ReasonForNoPeerName::NoTlsFromRemote => write!(f, "no_tls_from_remote"),
            ReasonForNoPeerName::NoPeerIdFromRemote => write!(f, "no_peer_id_from_remote"),
            ReasonForNoPeerName::NotProvidedByServiceDiscovery => {
                write!(f, "not_provided_by_service_discovery")
            }
            ReasonForNoPeerName::IngressNonHttp => write!(f, "ingress_non_http"),
        }
    }
}
