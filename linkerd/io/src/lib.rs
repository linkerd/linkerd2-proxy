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
use std::{mem::MaybeUninit, net::SocketAddr, pin::Pin, task::Context};
pub use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

pub type Poll<T> = std::task::Poll<Result<T>>;

pub trait PeerAddr {
    fn peer_addr(&self) -> SocketAddr;
}

impl PeerAddr for tokio::net::TcpStream {
    fn peer_addr(&self) -> SocketAddr {
        tokio::net::TcpStream::peer_addr(self).expect("TcpStream must have a peer address")
    }
}

impl<T: PeerAddr> PeerAddr for tokio_rustls::client::TlsStream<T> {
    fn peer_addr(&self) -> SocketAddr {
        self.get_ref().0.peer_addr()
    }
}

impl<T: PeerAddr> PeerAddr for tokio_rustls::server::TlsStream<T> {
    fn peer_addr(&self) -> SocketAddr {
        self.get_ref().0.peer_addr()
    }
}

#[cfg(feature = "tokio-test")]
impl PeerAddr for tokio_test::io::Mock {
    fn peer_addr(&self) -> SocketAddr {
        ([0, 0, 0, 0], 0).into()
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
        crate::poll_read_buf(cx, self, &mut buf)
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
        crate::poll_write_buf(cx, self, &mut buf)
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

// Vendored from upstream Tokio implementation, which is currently private.
// https://github.com/tokio-rs/tokio/blob/24ed874e81fa27a6505613051955127ba8ddfdf2/tokio-util/src/lib.rs#L72-L100
//
// TODO(eliza): if tokio makes this public, it would be nice to not vendor it.
pub fn poll_read_buf<T: AsyncRead>(
    cx: &mut Context<'_>,
    io: Pin<&mut T>,
    buf: &mut impl BufMut,
) -> Poll<usize> {
    if !buf.has_remaining_mut() {
        return Poll::Ready(Ok(0));
    }

    let n = {
        let dst = buf.bytes_mut();
        let dst = unsafe { &mut *(dst as *mut _ as *mut [MaybeUninit<u8>]) };
        let mut buf = ReadBuf::uninit(dst);
        let ptr = buf.filled().as_ptr();
        ready!(io.poll_read(cx, &mut buf)?);

        // Ensure the pointer does not change from under us
        assert_eq!(ptr, buf.filled().as_ptr());
        buf.filled().len()
    };

    // Safety: This is guaranteed to be the number of initialized (and read)
    // bytes due to the invariants provided by `ReadBuf::filled`.
    unsafe {
        buf.advance_mut(n);
    }

    Poll::Ready(Ok(n))
}

pub fn poll_write_buf<T: AsyncWrite>(
    cx: &mut Context<'_>,
    io: Pin<&mut T>,
    buf: &mut impl Buf,
) -> Poll<usize> {
    if !buf.has_remaining() {
        return Poll::Ready(Ok(0));
    }

    let n = ready!(io.poll_write(cx, buf.bytes()))?;
    buf.advance(n);
    Poll::Ready(Ok(n))
}
