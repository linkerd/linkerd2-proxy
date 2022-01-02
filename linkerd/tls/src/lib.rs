#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]

pub mod client;
pub mod server;

pub use linkerd_identity::LocalId;
use linkerd_io as io;

pub use self::{
    client::{Client, ClientTls, ConditionalClientTls, ConnectMeta, NoClientTls, ServerId},
    server::{ClientId, ConditionalServerTls, NewDetectTls, NoServerTls, ServerTls},
};

/// A trait implemented by transport streams to indicate its negotiated protocol.
pub trait HasNegotiatedProtocol {
    fn negotiated_protocol(&self) -> Option<NegotiatedProtocolRef<'_>>;
}

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

impl HasNegotiatedProtocol for tokio::net::TcpStream {
    #[inline]
    fn negotiated_protocol(&self) -> Option<NegotiatedProtocolRef<'_>> {
        None
    }
}

impl<I: HasNegotiatedProtocol> HasNegotiatedProtocol for io::ScopedIo<I> {
    #[inline]
    fn negotiated_protocol(&self) -> Option<NegotiatedProtocolRef<'_>> {
        self.get_ref().negotiated_protocol()
    }
}

impl<L, R> HasNegotiatedProtocol for io::EitherIo<L, R>
where
    L: HasNegotiatedProtocol,
    R: HasNegotiatedProtocol,
{
    #[inline]
    fn negotiated_protocol(&self) -> Option<NegotiatedProtocolRef<'_>> {
        match self {
            io::EitherIo::Left(l) => l.negotiated_protocol(),
            io::EitherIo::Right(r) => r.negotiated_protocol(),
        }
    }
}

/// Needed for tests.
impl HasNegotiatedProtocol for io::BoxedIo {
    #[inline]
    fn negotiated_protocol(&self) -> Option<NegotiatedProtocolRef<'_>> {
        None
    }
}
