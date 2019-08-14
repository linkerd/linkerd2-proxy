use std::io;
use std::net::{Shutdown, SocketAddr};

use bytes::Buf;
use futures::Poll;
use tokio::io::{AsyncRead, AsyncWrite};

use self::internal::Io;
use super::{AddrInfo, SetKeepalive};

/// A public wrapper around a `Box<Io>`.
///
/// This type ensures that the proper write_buf method is called,
/// to allow vectored writes to occur.
#[derive(Debug)]
pub struct BoxedIo(Box<dyn Io>);

impl BoxedIo {
    pub fn new<T: Io + 'static>(io: T) -> Self {
        BoxedIo(Box::new(io))
    }

    /// Since `Io` isn't publicly exported, but `Connection` wants
    /// this method, it's just an inherent method.
    pub fn shutdown_write(&mut self) -> Result<(), io::Error> {
        self.0.shutdown_write()
    }
}

impl io::Read for BoxedIo {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl io::Write for BoxedIo {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl AsyncRead for BoxedIo {
    unsafe fn prepare_uninitialized_buffer(&self, buf: &mut [u8]) -> bool {
        self.0.prepare_uninitialized_buffer(buf)
    }
}

impl AsyncWrite for BoxedIo {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        self.0.shutdown()
    }

    fn write_buf<B: Buf>(&mut self, mut buf: &mut B) -> Poll<usize, io::Error> {
        // A trait object of AsyncWrite would use the default write_buf,
        // which doesn't allow vectored writes. Going through this method
        // allows the trait object to call the specialized write_buf method.
        self.0.write_buf_erased(&mut buf)
    }
}

impl AddrInfo for BoxedIo {
    fn remote_addr(&self) -> Result<SocketAddr, io::Error> {
        self.0.remote_addr()
    }

    fn local_addr(&self) -> Result<SocketAddr, io::Error> {
        self.0.local_addr()
    }

    fn get_original_dst(&self) -> Option<SocketAddr> {
        self.0.get_original_dst()
    }
}

impl SetKeepalive for BoxedIo {
    fn keepalive(&self) -> io::Result<Option<::std::time::Duration>> {
        self.0.keepalive()
    }

    fn set_keepalive(&mut self, ka: Option<::std::time::Duration>) -> io::Result<()> {
        self.0.set_keepalive(ka)
    }
}

pub(super) mod internal {
    use super::{AddrInfo, AsyncRead, AsyncWrite, Buf, Poll, SetKeepalive, Shutdown};
    use std::io;
    use tokio::net::TcpStream;

    /// This trait is private, since it's purpose is for creating a dynamic
    /// trait object, but doing so without care can lead not getting vectored
    /// writes.
    ///
    /// Instead, used the concrete `BoxedIo` type.
    pub trait Io: AddrInfo + AsyncRead + AsyncWrite + SetKeepalive + Send {
        fn shutdown_write(&mut self) -> io::Result<()>;

        /// This method is to allow using `Async::write_buf` even through a
        /// trait object.
        fn write_buf_erased(&mut self, buf: &mut dyn Buf) -> Poll<usize, io::Error>;
    }

    impl Io for TcpStream {
        fn shutdown_write(&mut self) -> io::Result<()> {
            TcpStream::shutdown(self, Shutdown::Write)
        }

        fn write_buf_erased(&mut self, mut buf: &mut dyn Buf) -> Poll<usize, io::Error> {
            self.write_buf(&mut buf)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct WriteBufDetector;

    impl io::Read for WriteBufDetector {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            unreachable!("not called in test")
        }
    }

    impl io::Write for WriteBufDetector {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            panic!("BoxedIo called wrong write_buf method");
        }
        fn flush(&mut self) -> io::Result<()> {
            unreachable!("not called in test")
        }
    }

    impl AsyncRead for WriteBufDetector {}

    impl AsyncWrite for WriteBufDetector {
        fn shutdown(&mut self) -> Poll<(), io::Error> {
            unreachable!("not called in test")
        }

        fn write_buf<B: Buf>(&mut self, _: &mut B) -> Poll<usize, io::Error> {
            Ok(0.into())
        }
    }

    impl AddrInfo for WriteBufDetector {
        fn remote_addr(&self) -> Result<SocketAddr, io::Error> {
            unreachable!("not called in test")
        }

        fn local_addr(&self) -> Result<SocketAddr, io::Error> {
            unreachable!("not called in test")
        }

        fn get_original_dst(&self) -> Option<SocketAddr> {
            unreachable!("not called in test")
        }
    }

    impl SetKeepalive for WriteBufDetector {
        fn keepalive(&self) -> io::Result<Option<::std::time::Duration>> {
            unreachable!("not called in test")
        }

        fn set_keepalive(&mut self, _: Option<::std::time::Duration>) -> io::Result<()> {
            unreachable!("not called in test")
        }
    }

    impl Io for WriteBufDetector {
        fn shutdown_write(&mut self) -> Result<(), io::Error> {
            unreachable!("not called in test")
        }

        fn write_buf_erased(&mut self, mut buf: &mut dyn Buf) -> Poll<usize, io::Error> {
            self.write_buf(&mut buf)
        }
    }

    #[test]
    fn boxed_io_uses_vectored_io() {
        use bytes::IntoBuf;
        let mut io = BoxedIo::new(WriteBufDetector);

        // This method will trigger the panic in WriteBufDetector::write IFF
        // BoxedIo doesn't call write_buf_erased, but write_buf, and triggering
        // a regular write.
        io.write_buf(&mut "hello".into_buf()).expect("write_buf");
    }
}
