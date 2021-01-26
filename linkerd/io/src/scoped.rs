use crate as io;
use pin_project::pin_project;
use std::{pin::Pin, task::Context};

/// An I/O stream where errors are annotated a scope.
#[pin_project]
#[derive(Debug)]
pub struct ScopedIo<I> {
    scope: Scope,

    #[pin]
    io: I,
}

#[derive(Copy, Clone, Debug)]
enum Scope {
    Client,
    Server,
}

// === impl Scope ===

impl Scope {
    #[inline]
    fn err(&self) -> impl Fn(io::Error) -> io::Error {
        let scope = *self;
        move |err| io::Error::new(err.kind(), format!("{}: {}", scope, err))
    }
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Client => write!(f, "client"),
            Self::Server => write!(f, "server"),
        }
    }
}

// === impl ScopedIo ===

impl<I> ScopedIo<I> {
    pub fn client(io: I) -> Self {
        Self {
            scope: Scope::Client,
            io,
        }
    }

    pub fn server(io: I) -> Self {
        Self {
            scope: Scope::Server,
            io,
        }
    }

    pub fn get_ref(&self) -> &I {
        &self.io
    }

    pub fn get_mut(&mut self) -> &mut I {
        &mut self.io
    }
}

#[async_trait::async_trait]
impl<I: io::Peek + Send + Sync> io::Peek for ScopedIo<I> {
    async fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.io.peek(buf).await.map_err(self.scope.err())
    }
}

impl<I: io::PeerAddr> io::PeerAddr for ScopedIo<I> {
    #[inline]
    fn peer_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.io.peer_addr().map_err(self.scope.err())
    }
}

impl<I: io::AsyncRead> io::AsyncRead for ScopedIo<I> {
    #[inline]
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut io::ReadBuf<'_>,
    ) -> io::Poll<()> {
        let this = self.project();
        this.io.poll_read(cx, buf).map_err(this.scope.err())
    }
}

impl<I: io::Write> io::Write for ScopedIo<I> {
    #[inline]
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.io.write(buf).map_err(self.scope.err())
    }

    #[inline]
    fn flush(&mut self) -> io::Result<()> {
        self.io.flush().map_err(self.scope.err())
    }
}

impl<I: io::AsyncWrite> io::AsyncWrite for ScopedIo<I> {
    #[inline]
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> io::Poll<()> {
        let this = self.project();
        this.io.poll_shutdown(cx).map_err(this.scope.err())
    }

    #[inline]
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> io::Poll<()> {
        let this = self.project();
        this.io.poll_flush(cx).map_err(this.scope.err())
    }

    #[inline]
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> io::Poll<usize> {
        let this = self.project();
        let scope = this.scope;
        this.io.poll_write(cx, buf).map_err(scope.err())
    }

    #[inline]
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> io::Poll<usize> {
        let this = self.project();
        this.io
            .poll_write_vectored(cx, bufs)
            .map_err(this.scope.err())
    }

    #[inline]
    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }
}
