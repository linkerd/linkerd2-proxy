mod boxed;
mod either;
mod prefixed;
mod sensor;

pub use self::{
    boxed::BoxedIo,
    either::EitherIo,
    prefixed::PrefixedIo,
    sensor::{Sensor, SensorIo},
};
pub use std::io::*;
use std::net::SocketAddr;
pub use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
pub use tokio_util::io::{poll_read_buf, poll_write_buf};

pub type Poll<T> = std::task::Poll<Result<T>>;

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

#[cfg(feature = "tokio-test")]
impl PeerAddr for tokio::io::DuplexStream {
    fn peer_addr(&self) -> Result<SocketAddr> {
        Ok(([0, 0, 0, 0], 0).into())
    }
}

// === Peek ===

#[async_trait::async_trait]
pub trait Peek {
    async fn peek(&self, buf: &mut [u8]) -> Result<usize>;
}

// Special-case a wrapper for TcpStream::peek.
#[async_trait::async_trait]
impl Peek for tokio::net::TcpStream {
    async fn peek(&self, buf: &mut [u8]) -> Result<usize> {
        tokio::net::TcpStream::peek(self, buf).await
    }
}
