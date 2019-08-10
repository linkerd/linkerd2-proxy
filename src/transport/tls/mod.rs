pub mod client;
mod conditional_accept;
mod connection;
mod io;
pub mod listen;

pub use self::connection::Connection;
use self::io::TlsIo;
pub use self::listen::Listen;
use crate::identity;
pub use rustls::TLSError as Error;
use std::fmt;
use tokio_rustls::{Accept, TlsAcceptor as Acceptor, TlsConnector as Connector};

/// Describes whether or not a connection was secured with TLS and, if it was
/// not, the reason why.
pub type Conditional<T> = crate::Conditional<T, ReasonForNoIdentity>;

pub type PeerIdentity = Conditional<identity::Name>;
pub type Status = Conditional<()>;

pub trait HasPeerIdentity {
    fn peer_identity(&self) -> PeerIdentity;
}

pub trait HasStatus {
    fn tls_status(&self) -> Status;
}

impl<T: HasPeerIdentity> HasStatus for T {
    fn tls_status(&self) -> Status {
        self.peer_identity().map(|_| ())
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReasonForNoIdentity {
    /// Identity is administratively disabled.
    Disabled,

    /// The remote peer does not have a known identity name.
    NoPeerName(ReasonForNoPeerName),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReasonForNoPeerName {
    /// The connection is a non-HTTP connection so we don't know anything
    /// about the destination besides its address.
    NotHttp,

    /// The connection is for HTTP but the HTTP request doesn't have an
    /// authority so we can't extract the identity from it.
    NoAuthorityInHttpRequest,

    /// The destination service didn't give us the identity, which is its way
    /// of telling us that we shouldn't do TLS for this endpoint.
    NotProvidedByServiceDiscovery,

    /// No TLS is wanted because the connection is a loopback connection which
    /// doesn't need or support TLS.
    Loopback,

    // Identity was not provided by the remote peer.
    NotProvidedByRemote,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use crate::Conditional;
        match self {
            Conditional::Some(()) => write!(f, "true"),
            Conditional::None(r) => fmt::Display::fmt(&r, f),
        }
    }
}

impl From<ReasonForNoPeerName> for ReasonForNoIdentity {
    fn from(r: ReasonForNoPeerName) -> Self {
        ReasonForNoIdentity::NoPeerName(r)
    }
}

impl fmt::Display for ReasonForNoIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReasonForNoIdentity::Disabled => write!(f, "disabled"),
            ReasonForNoIdentity::NoPeerName(n) => write!(f, "{}", n),
        }
    }
}

impl fmt::Display for ReasonForNoPeerName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReasonForNoPeerName::Loopback => write!(f, "loopback"),
            ReasonForNoPeerName::NoAuthorityInHttpRequest => {
                write!(f, "no_authority_in_http_request")
            }
            ReasonForNoPeerName::NotHttp => write!(f, "not_http"),
            ReasonForNoPeerName::NotProvidedByRemote => write!(f, "not_provided_by_remote"),
            ReasonForNoPeerName::NotProvidedByServiceDiscovery => {
                write!(f, "not_provided_by_service_discovery")
            }
        }
    }
}
