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
use futures::ready;
pub use std::io::*;
use std::{net::SocketAddr, pin::Pin, task::Context};
pub use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
pub use tokio_util::io::poll_read_buf;

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

// Vendored from upstream:
// https://github.com/tokio-rs/tokio/blob/920bd4333c1ec563fa3a32498a26c021e8fb1fed/tokio-util/src/lib.rs#L177-L199
// which hasn't merged yet.
//
// TODO(eliza): depend on this from upstream when `tokio-rs/tokio#3156` merges.
pub fn poll_write_buf<T: AsyncWrite, B: Buf>(
    io: Pin<&mut T>,
    cx: &mut Context<'_>,
    buf: &mut B,
) -> Poll<usize> {
    const MAX_BUFS: usize = 64;

    if !buf.has_remaining() {
        return Poll::Ready(Ok(0));
    }

    let n = if io.is_write_vectored() {
        let mut slices = [IoSlice::new(&[]); MAX_BUFS];
        let cnt = buf.bytes_vectored(&mut slices);
        ready!(io.poll_write_vectored(cx, &slices[..cnt]))?
    } else {
        ready!(io.poll_write(cx, buf.bytes()))?
    };

    buf.advance(n);

    Poll::Ready(Ok(n))
}