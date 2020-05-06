use crate::internal::Io;
use bytes::{Buf, Bytes};
use std::task::{Context, Poll};
use std::{cmp, io};
use tokio::io::{AsyncRead, AsyncWrite};

/// A TcpStream where the initial reads will be served from `prefix`.
#[derive(Debug)]
pub struct PrefixedIo<S> {
    prefix: Bytes,
    io: S,
}

impl<S: AsyncRead + AsyncWrite> PrefixedIo<S> {
    pub fn new(prefix: impl Into<Bytes>, io: S) -> Self {
        let prefix = prefix.into();
        Self { prefix, io }
    }

    pub fn prefix(&self) -> &Bytes {
        &self.prefix
    }
}

impl<S: io::Read> io::Read for PrefixedIo<S> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, io::Error> {
        // Check the length only once, since looking as the length
        // of a Bytes isn't as cheap as the length of a &[u8].
        let peeked_len = self.prefix.len();

        if peeked_len == 0 {
            self.io.read(buf)
        } else {
            let len = cmp::min(buf.len(), peeked_len);
            buf[..len].copy_from_slice(&self.prefix.as_ref()[..len]);
            self.prefix.advance(len);
            // If we've finally emptied the prefix, drop it so we don't
            // hold onto the allocated memory any longer. We won't peek
            // again.
            if peeked_len == len {
                self.prefix = Default::default();
            }
            Ok(len)
        }
    }
}

impl<S: AsyncRead> AsyncRead for PrefixedIo<S> {
    unsafe fn prepare_uninitialized_buffer(&self, buf: &mut [u8]) -> bool {
        self.io.prepare_uninitialized_buffer(buf)
    }
}

impl<S: io::Write> io::Write for PrefixedIo<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.io.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.io.flush()
    }
}

impl<S: AsyncWrite> AsyncWrite for PrefixedIo<S> {
    fn shutdown(&mut self) -> super::Poll<()> {
        self.io.shutdown()
    }

    fn write_buf<B: Buf>(&mut self, buf: &mut B) -> Poll<usize, io::Error>
    where
        Self: Sized,
    {
        self.io.write_buf(buf)
    }
}

impl<S: Io> Io for PrefixedIo<S> {
    fn shutdown_write(&mut self) -> io::Result<()> {
        self.io.shutdown_write()
    }

    fn poll_write_buf_erased(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut dyn Buf,
    ) -> Poll<usize, io::Error> {
        self.io.poll_write_buf_erased(cx, buf)
    }
}
