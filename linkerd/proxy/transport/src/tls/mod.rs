use linkerd2_identity as identity;
pub use rustls::TLSError as Error;
use std::fmt;

pub mod accept;
pub mod client;
pub mod conditional_accept;

pub use self::accept::DetectTls;
pub use self::client::ConnectLayer;

/// Describes whether or not a connection was secured with TLS and, if it was
/// not, the reason why.
pub type Conditional<T> = linkerd2_conditional::Conditional<T, ReasonForNoPeerName>;

pub type PeerIdentity = Conditional<identity::Name>;

pub trait HasPeerIdentity {
    fn peer_identity(&self) -> PeerIdentity;
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReasonForNoPeerName {
    /// The connection is a non-HTTP connection so we don't know anything
    /// about the destination besides its address.
    OpaqueTcp,

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

    /// Identity is administratively disabled.
    LocalIdentityDisabled,
}

impl fmt::Display for ReasonForNoPeerName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReasonForNoPeerName::LocalIdentityDisabled => write!(f, "disabled"),
            ReasonForNoPeerName::Loopback => write!(f, "loopback"),
            ReasonForNoPeerName::OpaqueTcp => write!(f, "opaque_tcp"),
            ReasonForNoPeerName::PortSkipped => write!(f, "port_skipped"),
            ReasonForNoPeerName::NoTlsFromRemote => write!(f, "no_tls_from_remote"),
            ReasonForNoPeerName::NoPeerIdFromRemote => write!(f, "no_peer_id_from_remote"),
            ReasonForNoPeerName::NotProvidedByServiceDiscovery => {
                write!(f, "not_provided_by_service_discovery")
            }
        }
    }
}
