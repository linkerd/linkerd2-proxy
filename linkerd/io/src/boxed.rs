use super::{internal::Io, AsyncRead, AsyncWrite, Poll, Result};
use futures::try_ready;

/// A public wrapper around a `Box<Io>`.
///
/// This type ensures that the proper write_buf method is called,
/// to allow vectored writes to occur.
pub struct BoxedIo(Box<dyn Io>);

impl BoxedIo {
    pub fn new<T: Io + 'static>(io: T) -> Self {
        BoxedIo(Box::new(io))
    }

    /// Since `Io` isn't publicly exported, but `Connection` wants
    /// this method, it's just an inherent method.
    pub fn shutdown_write(&mut self) -> Result<()> {
        self.0.shutdown_write()
    }
}

impl std::io::Read for BoxedIo {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        self.0.read(buf)
    }
}

impl std::io::Write for BoxedIo {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> Result<()> {
        self.0.flush()
    }
}

impl AsyncRead for BoxedIo {
    unsafe fn prepare_uninitialized_buffer(&self, buf: &mut [u8]) -> bool {
        self.0.prepare_uninitialized_buffer(buf)
    }
}

impl AsyncWrite for BoxedIo {
    fn shutdown(&mut self) -> Poll<()> {
        try_ready!(self.0.shutdown());

        // TCP shutdown the write side.
        //
        // If we're shutting down, then we definitely won't write
        // anymore. So, we should tell the remote about this. This
        // is relied upon in our TCP proxy, to start shutting down
        // the pipe if one side closes.
        Ok(self.0.shutdown_write()?.into())
    }

    fn write_buf<B: bytes::Buf>(&mut self, mut buf: &mut B) -> Poll<usize> {
        // A trait object of AsyncWrite would use the default write_buf,
        // which doesn't allow vectored writes. Going through this method
        // allows the trait object to call the specialized write_buf method.
        self.0.write_buf_erased(&mut buf)
    }
}

impl Io for BoxedIo {
    fn shutdown_write(&mut self) -> Result<()> {
        self.0.shutdown_write()
    }

    fn write_buf_erased(&mut self, buf: &mut dyn bytes::Buf) -> Poll<usize> {
        self.0.write_buf_erased(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct WriteBufDetector;

    impl std::io::Read for WriteBufDetector {
        fn read(&mut self, _: &mut [u8]) -> Result<usize> {
            unreachable!("not called in test")
        }
    }

    impl std::io::Write for WriteBufDetector {
        fn write(&mut self, _: &[u8]) -> Result<usize> {
            panic!("BoxedIo called wrong write_buf method");
        }
        fn flush(&mut self) -> Result<()> {
            unreachable!("not called in test")
        }
    }

    impl AsyncRead for WriteBufDetector {}

    impl AsyncWrite for WriteBufDetector {
        fn shutdown(&mut self) -> Poll<()> {
            unreachable!("not called in test")
        }

        fn write_buf<B: bytes::Buf>(&mut self, _: &mut B) -> Poll<usize> {
            Ok(0.into())
        }
    }

    impl Io for WriteBufDetector {
        fn shutdown_write(&mut self) -> Result<()> {
            unreachable!("not called in test")
        }

        fn write_buf_erased(&mut self, mut buf: &mut dyn bytes::Buf) -> Poll<usize> {
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
