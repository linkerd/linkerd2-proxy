#![deny(warnings, rust_2018_idioms)]

pub use linkerd_identity::LocalId;
use linkerd_io as io;
pub use rustls::Session;

pub mod client;
pub mod server;

pub use self::{
    client::{Client, ClientTls, ConditionalClientTls, NoClientTls, ServerId},
    server::{ClientId, ConditionalServerTls, NewDetectTls, NoServerTls, ServerTls},
};

/// Indicates a negotiated protocol.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct NegotiatedProtocol<'t>(pub &'t [u8]);

/// Indicates a negotiated protocol.
pub trait HasNegotiatedProtocol {
    fn negotiated_protocol(&self) -> Option<NegotiatedProtocol<'_>>;
}

impl<I> HasNegotiatedProtocol for self::client::TlsStream<I> {
    #[inline]
    fn negotiated_protocol(&self) -> Option<NegotiatedProtocol<'_>> {
        self.get_ref().1.get_alpn_protocol().map(NegotiatedProtocol)
    }
}

impl<I> HasNegotiatedProtocol for self::server::TlsStream<I> {
    #[inline]
    fn negotiated_protocol(&self) -> Option<NegotiatedProtocol<'_>> {
        self.get_ref().1.get_alpn_protocol().map(NegotiatedProtocol)
    }
}

impl HasNegotiatedProtocol for tokio::net::TcpStream {
    #[inline]
    fn negotiated_protocol(&self) -> Option<NegotiatedProtocol<'_>> {
        None
    }
}

impl<I: HasNegotiatedProtocol> HasNegotiatedProtocol for io::ScopedIo<I> {
    #[inline]
    fn negotiated_protocol(&self) -> Option<NegotiatedProtocol<'_>> {
        self.get_ref().negotiated_protocol()
    }
}

impl<L, R> HasNegotiatedProtocol for io::EitherIo<L, R>
where
    L: HasNegotiatedProtocol,
    R: HasNegotiatedProtocol,
{
    #[inline]
    fn negotiated_protocol(&self) -> Option<NegotiatedProtocol<'_>> {
        match self {
            io::EitherIo::Left(l) => l.negotiated_protocol(),
            io::EitherIo::Right(r) => r.negotiated_protocol(),
        }
    }
}
