use crate::{Io, IoSlice, PeerAddr, Poll};
use bytes::{Buf, Bytes};
use pin_project::pin_project;
use std::{cmp, io};
use std::{pin::Pin, task::Context};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf, Result};

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
    fn peer_addr(&self) -> Result<std::net::SocketAddr> {
        self.io.peer_addr()
    }
}

impl<S: AsyncRead> AsyncRead for PrefixedIo<S> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<()> {
        let this = self.project();
        // Check the length only once, since looking as the length
        // of a Bytes isn't as cheap as the length of a &[u8].
        let peeked_len = this.prefix.len();

        if peeked_len == 0 {
            this.io.poll_read(cx, buf)
        } else {
            let len = cmp::min(buf.remaining(), peeked_len);
            buf.put_slice(&this.prefix.as_ref()[..len]);
            this.prefix.advance(len);
            // If we've finally emptied the prefix, drop it so we don't
            // hold onto the allocated memory any longer. We won't peek
            // again.
            if peeked_len == len {
                *this.prefix = Bytes::new();
            }
            Poll::Ready(Ok(()))
        }
    }
}

impl<S: io::Write> io::Write for PrefixedIo<S> {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        self.io.write(buf)
    }

    fn flush(&mut self) -> Result<()> {
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

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<usize> {
        self.project().io.poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }
}

impl<S: Io> crate::internal::Sealed for PrefixedIo<S> {}
