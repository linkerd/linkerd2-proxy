use crate::{self as io};
use bytes::{Buf, Bytes};
use pin_project::pin_project;
use std::{cmp, pin::Pin, task::Context};

/// A TcpStream where the initial reads will be served from `prefix`.
#[pin_project]
#[derive(Debug)]
pub struct PrefixedIo<I> {
    prefix: Bytes,

    #[pin]
    io: I,
}

impl<I> PrefixedIo<I> {
    pub fn new(prefix: impl Into<Bytes>, io: I) -> Self {
        let prefix = prefix.into();
        Self { prefix, io }
    }

    pub fn prefix(&self) -> &Bytes {
        &self.prefix
    }
}

impl<I> From<I> for PrefixedIo<I> {
    fn from(io: I) -> Self {
        Self::new(Bytes::default(), io)
    }
}

#[async_trait::async_trait]
impl<I: io::Peek + Send + Sync> io::Peek for PrefixedIo<I> {
    async fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        let sz = self.prefix.len().min(buf.len());
        if sz == 0 {
            return self.io.peek(buf).await;
        }

        (&mut buf[..sz]).clone_from_slice(&self.prefix[..sz]);
        Ok(sz)
    }
}

impl<I: io::PeerAddr> io::PeerAddr for PrefixedIo<I> {
    #[inline]
    fn peer_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.io.peer_addr()
    }
}

impl<I: io::AsyncRead> io::AsyncRead for PrefixedIo<I> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut io::ReadBuf<'_>,
    ) -> io::Poll<()> {
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
            io::Poll::Ready(Ok(()))
        }
    }
}

impl<I: io::Write> io::Write for PrefixedIo<I> {
    #[inline]
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.io.write(buf)
    }

    #[inline]
    fn flush(&mut self) -> io::Result<()> {
        self.io.flush()
    }
}

impl<I: io::AsyncWrite> io::AsyncWrite for PrefixedIo<I> {
    #[inline]
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> io::Poll<()> {
        self.project().io.poll_shutdown(cx)
    }

    #[inline]
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> io::Poll<()> {
        self.project().io.poll_flush(cx)
    }

    #[inline]
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> io::Poll<usize> {
        self.project().io.poll_write(cx, buf)
    }

    #[inline]
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> io::Poll<usize> {
        self.project().io.poll_write_vectored(cx, bufs)
    }

    #[inline]
    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }
}
