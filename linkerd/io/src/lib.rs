mod boxed;
mod peek;
mod prefixed;
mod sensor;

pub use self::{
    boxed::BoxedIo,
    peek::{Peek, Peekable},
    prefixed::PrefixedIo,
    sensor::{Sensor, SensorIo},
};
use bytes::{Buf, BufMut};
pub use std::io::*;
use std::{net::SocketAddr, pin::Pin, task::Context};
pub use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
pub use tokio_util::io::{poll_read_buf, poll_write_buf};

pub type Poll<T> = std::task::Poll<Result<T>>;

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

/// This trait is private, since its purpose is for creating a dynamic trait
/// object, but doing so without care can to lead not getting vectored
/// writes.
///
/// Instead, use the concrete `BoxedIo` type.
pub trait Io: AsyncRead + AsyncWrite + PeerAddr + Send + internal::Sealed {
    /// This method is to allow using `Async::polL_read_buf` even through a
    /// trait object.
    #[inline]
    fn poll_read_buf_erased(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: &mut dyn BufMut,
    ) -> Poll<usize>
    where
        Self: Sized,
    {
        crate::poll_read_buf(self, cx, &mut buf)
    }

    /// This method is to allow using `Async::poll_write_buf` even through a
    /// trait object.
    #[inline]
    fn poll_write_buf_erased(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: &mut dyn Buf,
    ) -> Poll<usize>
    where
        Self: Sized,
    {
        crate::poll_write_buf(self, cx, &mut buf)
    }
}

impl<I> Io for I where I: AsyncRead + AsyncWrite + PeerAddr + Send + internal::Sealed {}
mod internal {
    use super::Io;
    pub trait Sealed {}
    impl Sealed for tokio::net::TcpStream {}
    impl<S: Io + Unpin> Sealed for tokio_rustls::server::TlsStream<S> {}
    impl<S: Io + Unpin> Sealed for tokio_rustls::client::TlsStream<S> {}
}
