use crate::{internal::Io, PeerAddr, Poll};
use bytes::{Buf, BufMut, Bytes};
use std::{cmp, io};
use std::{mem::MaybeUninit, pin::Pin, task::Context};
use tokio::io::{AsyncRead, AsyncWrite};

use pin_project::pin_project;

/// A TcpStream where the initial reads will be served from `prefix`.
#[pin_project]
#[derive(Debug)]
pub struct PrefixedIo<S> {
    prefix: Bytes,

    #[pin]
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

impl<S: PeerAddr> PeerAddr for PrefixedIo<S> {
    fn peer_addr(&self) -> std::net::SocketAddr {
        self.io.peer_addr()
    }
}

impl<S: AsyncRead> AsyncRead for PrefixedIo<S> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<usize> {
        let this = self.project();
        // Check the length only once, since looking as the length
        // of a Bytes isn't as cheap as the length of a &[u8].
        let peeked_len = this.prefix.len();

        if peeked_len == 0 {
            this.io.poll_read(cx, buf)
        } else {
            let len = cmp::min(buf.len(), peeked_len);
            buf[..len].copy_from_slice(&this.prefix.as_ref()[..len]);
            this.prefix.advance(len);
            // If we've finally emptied the prefix, drop it so we don't
            // hold onto the allocated memory any longer. We won't peek
            // again.
            if peeked_len == len {
                *this.prefix = Bytes::new();
            }
            Poll::Ready(Ok(len))
        }
    }

    unsafe fn prepare_uninitialized_buffer(&self, buf: &mut [MaybeUninit<u8>]) -> bool {
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
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.project().io.poll_shutdown(cx)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.project().io.poll_flush(cx)
    }

    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<usize> {
        self.project().io.poll_write(cx, buf)
    }

    fn poll_write_buf<B: Buf>(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut B,
    ) -> Poll<usize>
    where
        Self: Sized,
    {
        self.project().io.poll_write_buf(cx, buf)
    }
}

impl<S: Io> Io for PrefixedIo<S> {
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
