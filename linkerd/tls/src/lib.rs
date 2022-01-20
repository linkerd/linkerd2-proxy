#![deny(warnings, rust_2018_idioms, clippy::disallowed_method)]
#![forbid(unsafe_code)]

pub mod client;
pub mod server;

pub use linkerd_identity::LocalId;

pub use self::{
    client::{Client, ClientTls, ConditionalClientTls, ConnectMeta, NoClientTls, ServerId},
    server::{ClientId, ConditionalServerTls, NewDetectTls, NoServerTls, ServerTls},
};

#[derive(Clone, Eq, PartialEq, Hash)]
pub struct NegotiatedProtocol(pub Vec<u8>);

/// Indicates a negotiated protocol.
#[derive(Copy, Clone, Eq, PartialEq, Hash)]
pub struct NegotiatedProtocolRef<'t>(pub &'t [u8]);

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
