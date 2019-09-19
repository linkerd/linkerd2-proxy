pub mod client;
mod connection;
mod detect_sni;
mod io;
pub mod listen;

pub use self::connection::{Connection, Tls};
pub use self::listen::Listen;
use crate::identity;
pub use rustls::TLSError as Error;
use std::fmt;
use tokio_rustls::{Accept, TlsAcceptor as Acceptor, TlsConnector as Connector};

/// Describes whether or not a connection was secured with TLS and, if it was
/// not, the reason why.
pub type Conditional<T> = crate::Conditional<T, ReasonForNoIdentity>;

pub type PeerIdentity = Conditional<identity::Name>;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Status(Conditional<Tls>);

pub trait HasPeerIdentity {
    fn peer_identity(&self) -> PeerIdentity;
}

pub trait HasStatus {
    fn tls_status(&self) -> Status;
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

impl From<Conditional<Tls>> for Status {
    fn from(inner: Conditional<Tls>) -> Self {
        Status(inner)
    }
}

impl Into<Conditional<Tls>> for Status {
    fn into(self) -> Conditional<Tls> {
        self.0
    }
}

impl<T: HasStatus> HasPeerIdentity for T {
    fn peer_identity(&self) -> PeerIdentity {
        self.tls_status().0.and_then(|status| match status {
            Tls::Established { peer } => peer.clone(),
            Tls::Opaque { .. } => {
                Conditional::None(ReasonForNoPeerName::NotProvidedByRemote.into())
            }
        })
    }
}

impl Status {
    pub fn as_ref(&self) -> Conditional<&Tls> {
        self.0.as_ref()
    }

    pub fn is_tls(&self) -> bool {
        self.0.is_some()
    }

    pub fn no_tls_reason(&self) -> Option<ReasonForNoIdentity> {
        self.0.reason()
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
            ReasonForNoIdentity::NoPeerName(n) => n.fmt(f),
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
