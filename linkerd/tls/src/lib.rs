#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

pub mod client;
pub mod detect_sni;
pub mod server;

pub use self::{
    client::{Client, ClientTls, ConditionalClientTls, ConnectMeta, NoClientTls, ServerId},
    detect_sni::{DetectSni, NewDetectSni},
    server::{ClientId, ConditionalServerTls, NewDetectTls, NoServerTls, ServerTls},
};

use linkerd_dns_name as dns;

/// Describes the the Server Name Indication (SNI) value used by both clients
/// and servers.
///
/// Clients use this type to describe the SNI value to be sent to a server.
///
/// Servers use this type to describe the SNI value that they expect from
/// clients.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ServerName(pub dns::Name);

#[derive(Clone, Eq, PartialEq, Hash)]
pub struct NegotiatedProtocol(pub Vec<u8>);

/// Indicates a negotiated protocol.
#[derive(Copy, Clone, Eq, PartialEq, Hash)]
pub struct NegotiatedProtocolRef<'t>(pub &'t [u8]);

// === impl NegotiatedProtocol ===

impl NegotiatedProtocol {
    pub fn as_ref(&self) -> NegotiatedProtocolRef<'_> {
        NegotiatedProtocolRef(&self.0)
    }
}

impl std::fmt::Debug for NegotiatedProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        NegotiatedProtocolRef(&self.0).fmt(f)
    }
}

impl NegotiatedProtocolRef<'_> {
    pub fn to_owned(self) -> NegotiatedProtocol {
        NegotiatedProtocol(self.0.into())
    }
}

impl From<NegotiatedProtocolRef<'_>> for NegotiatedProtocol {
    fn from(protocol: NegotiatedProtocolRef<'_>) -> NegotiatedProtocol {
        protocol.to_owned()
    }
}

impl std::fmt::Debug for NegotiatedProtocolRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match std::str::from_utf8(self.0) {
            Ok(s) => s.fmt(f),
            Err(_) => self.0.fmt(f),
        }
    }
}

// === impl ServerName ===

impl From<dns::Name> for ServerName {
    fn from(n: dns::Name) -> Self {
        Self(n)
    }
}

impl From<ServerName> for dns::Name {
    fn from(ServerName(name): ServerName) -> dns::Name {
        name
    }
}

impl std::ops::Deref for ServerName {
    type Target = dns::Name;

    fn deref(&self) -> &dns::Name {
        &self.0
    }
}

impl std::str::FromStr for ServerName {
    type Err = dns::InvalidName;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        dns::Name::from_str(s).map(ServerName)
    }
}

impl std::fmt::Display for ServerName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
