mod boxed;
mod peek;
mod prefixed;
pub use self::{boxed::BoxedIo, peek::Peek, prefixed::PrefixedIo};
pub use std::io::{Error, Read, Result, Write};
pub use tokio::io::{AsyncRead, AsyncWrite};

pub type Poll<T> = std::task::Poll<Result<T>>;

pub trait Peekable {
    fn peek(self, capacity: usize) -> Peek<Self>
    where
        Self: Sized + internal::Io + Unpin,
    {
        Peek::with_capacity(capacity, self)
    }
}

mod internal {
    use super::{AsyncRead, AsyncWrite, Peekable, Poll};
    use bytes::{Buf, BufMut};
    use std::pin::Pin;
    use std::task::Context;

    /// This trait is private, since its purpose is for creating a dynamic trait
    /// object, but doing so without care can to lead not getting vectored
    /// writes.
    ///
    /// Instead, use the concrete `BoxedIo` type.
    pub trait Io: AsyncRead + AsyncWrite + Send {
        /// This method is to allow using `Async::polL_read_buf` even through a
        /// trait object.
        fn poll_read_buf_erased(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut dyn BufMut,
        ) -> Poll<usize>;

        /// This method is to allow using `Async::poll_write_buf` even through a
        /// trait object.
        fn poll_write_buf_erased(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut dyn Buf,
        ) -> Poll<usize>;
    }

    impl<T: Io> Peekable for T {}

    impl Io for tokio::net::TcpStream {
        fn poll_write_buf_erased(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            mut buf: &mut dyn Buf,
        ) -> Poll<usize> {
            self.poll_write_buf(cx, &mut buf)
        }

        fn poll_read_buf_erased(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            mut buf: &mut dyn BufMut,
        ) -> Poll<usize> {
            self.poll_read_buf(cx, &mut buf)
        }
    }

    impl<S: Io + Unpin> Io for tokio_rustls::server::TlsStream<S> {
        fn poll_write_buf_erased(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            mut buf: &mut dyn Buf,
        ) -> Poll<usize> {
            self.poll_write_buf(cx, &mut buf)
        }

        fn poll_read_buf_erased(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            mut buf: &mut dyn BufMut,
        ) -> Poll<usize> {
            self.poll_read_buf(cx, &mut buf)
        }
    }

    impl<S: Io + Unpin> Io for tokio_rustls::client::TlsStream<S> {
        fn poll_write_buf_erased(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            mut buf: &mut dyn Buf,
        ) -> Poll<usize> {
            self.poll_write_buf(cx, &mut buf)
        }

        fn poll_read_buf_erased(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            mut buf: &mut dyn BufMut,
        ) -> Poll<usize> {
            self.poll_read_buf(cx, &mut buf)
        }
    }
}
