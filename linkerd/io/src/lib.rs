#![deny(
    warnings,
    rust_2018_idioms,
    clippy::disallowed_methods,
    clippy::disallowed_types
)]
#![forbid(unsafe_code)]

mod boxed;
mod either;
mod prefixed;
mod scoped;
mod sensor;

pub use self::{
    boxed::BoxedIo,
    either::EitherIo,
    prefixed::PrefixedIo,
    scoped::ScopedIo,
    sensor::{Sensor, SensorIo},
};
pub use std::io::*;
use std::net::SocketAddr;
pub use tokio::io::{
    duplex, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf,
};
pub use tokio_util::io::{poll_read_buf, poll_write_buf};

pub type Poll<T> = std::task::Poll<Result<T>>;

// === Peek ===

#[async_trait::async_trait]
pub trait Peek {
    /// Receives data on the socket from the remote address to which it is
    /// connected, without removing that data from the queue. On success,
    /// returns the number of bytes peeked. A return value of zero bytes does not
    /// necessarily indicate that the underlying socket has closed.
    ///
    /// Successive calls return the same data. This is accomplished by passing
    /// `MSG_PEEK` as a flag to the underlying recv system call.
    async fn peek(&self, buf: &mut [u8]) -> Result<usize>;
}

// Special-case a wrapper for TcpStream::peek.
#[async_trait::async_trait]
impl Peek for tokio::net::TcpStream {
    async fn peek(&self, buf: &mut [u8]) -> Result<usize> {
        tokio::net::TcpStream::peek(self, buf).await
    }
}

#[async_trait::async_trait]
impl Peek for tokio::io::DuplexStream {
    async fn peek(&self, _: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
}

// === PeerAddr ===

pub trait PeerAddr {
    fn peer_addr(&self) -> Result<SocketAddr>;
}

impl PeerAddr for tokio::net::TcpStream {
    fn peer_addr(&self) -> Result<SocketAddr> {
        tokio::net::TcpStream::peer_addr(self)
    }
}

impl<T: PeerAddr> PeerAddr for tokio_rustls::client::TlsStream<T> {
    fn peer_addr(&self) -> Result<SocketAddr> {
        self.get_ref().0.peer_addr()
    }
}

impl<T: PeerAddr> PeerAddr for tokio_rustls::server::TlsStream<T> {
    fn peer_addr(&self) -> Result<SocketAddr> {
        self.get_ref().0.peer_addr()
    }
}

#[cfg(feature = "tokio-test")]
impl PeerAddr for tokio_test::io::Mock {
    fn peer_addr(&self) -> Result<SocketAddr> {
        Ok(([0, 0, 0, 0], 0).into())
    }
}

impl PeerAddr for tokio::io::DuplexStream {
    fn peer_addr(&self) -> Result<SocketAddr> {
        Ok(([0, 0, 0, 0], 0).into())
    }
}
