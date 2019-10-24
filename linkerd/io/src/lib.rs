mod boxed;
mod peek;
mod prefixed;

pub use self::{boxed::BoxedIo, peek::Peek, prefixed::PrefixedIo};
pub use std::io::{Error, Read, Result, Write};
pub use tokio::io::{AsyncRead, AsyncWrite};

pub type Poll<T> = futures::Poll<T, Error>;

mod internal {
    use super::{AsyncRead, AsyncWrite, Poll, Result};
    use bytes::Buf;
    use std::net::Shutdown;

    /// This trait is private, since it's purpose is for creating a dynamic trait
    /// object, but doing so without care can lead to not getting vectored
    /// writes.
    ///
    /// Instead, use the concrete `BoxedIo` type.
    pub trait Io: AsyncRead + AsyncWrite + Send {
        fn shutdown_write(&mut self) -> Result<()>;

        /// This method is to allow using `Async::write_buf` even through a
        /// trait object.
        fn write_buf_erased(&mut self, buf: &mut dyn Buf) -> Poll<usize>;
    }

    impl Io for tokio::net::TcpStream {
        fn shutdown_write(&mut self) -> Result<()> {
            tokio::net::TcpStream::shutdown(self, Shutdown::Write)
        }

        fn write_buf_erased(&mut self, mut buf: &mut dyn Buf) -> Poll<usize> {
            self.write_buf(&mut buf)
        }
    }

    impl<S: Io> Io for tokio_rustls::server::TlsStream<S> {
        fn shutdown_write(&mut self) -> Result<()> {
            self.get_mut().0.shutdown_write()
        }

        fn write_buf_erased(&mut self, mut buf: &mut dyn Buf) -> Poll<usize> {
            self.write_buf(&mut buf)
        }
    }

    impl<S: Io> Io for tokio_rustls::client::TlsStream<S> {
        fn shutdown_write(&mut self) -> Result<()> {
            self.get_mut().0.shutdown_write()
        }

        fn write_buf_erased(&mut self, mut buf: &mut dyn Buf) -> Poll<usize> {
            self.write_buf(&mut buf)
        }
    }
}
