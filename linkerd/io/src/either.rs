use crate as io;
use pin_project::pin_project;
use std::{pin::Pin, task::Context};

#[pin_project(project = EitherIoProj)]
#[derive(Debug)]
pub enum EitherIo<L, R> {
    Left(#[pin] L),
    Right(#[pin] R),
}

#[async_trait::async_trait]
impl<L, R> io::Peek for EitherIo<L, R>
where
    L: io::Peek + Send + Sync,
    R: io::Peek + Send + Sync,
{
    async fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Left(l) => l.peek(buf).await,
            Self::Right(r) => r.peek(buf).await,
        }
    }
}

impl<L: io::PeerAddr, R: io::PeerAddr> io::PeerAddr for EitherIo<L, R> {
    #[inline]
    fn peer_addr(&self) -> io::Result<std::net::SocketAddr> {
        match self {
            Self::Left(l) => l.peer_addr(),
            Self::Right(r) => r.peer_addr(),
        }
    }
}

impl<L: io::AsyncRead, R: io::AsyncRead> io::AsyncRead for EitherIo<L, R> {
    #[inline]
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut io::ReadBuf<'_>,
    ) -> io::Poll<()> {
        match self.project() {
            EitherIoProj::Left(l) => l.poll_read(cx, buf),
            EitherIoProj::Right(r) => r.poll_read(cx, buf),
        }
    }
}

impl<L: io::AsyncWrite, R: io::AsyncWrite> io::AsyncWrite for EitherIo<L, R> {
    #[inline]
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context) -> io::Poll<()> {
        match self.project() {
            EitherIoProj::Left(l) => l.poll_shutdown(cx),
            EitherIoProj::Right(r) => r.poll_shutdown(cx),
        }
    }

    #[inline]
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> io::Poll<()> {
        match self.project() {
            EitherIoProj::Left(l) => l.poll_flush(cx),
            EitherIoProj::Right(r) => r.poll_flush(cx),
        }
    }

    #[inline]
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> io::Poll<usize> {
        match self.project() {
            EitherIoProj::Left(l) => l.poll_write(cx, buf),
            EitherIoProj::Right(r) => r.poll_write(cx, buf),
        }
    }

    #[inline]
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &[io::IoSlice<'_>],
    ) -> io::Poll<usize> {
        match self.project() {
            EitherIoProj::Left(l) => l.poll_write_vectored(cx, buf),
            EitherIoProj::Right(r) => r.poll_write_vectored(cx, buf),
        }
    }

    #[inline]
    fn is_write_vectored(&self) -> bool {
        match self {
            EitherIo::Left(l) => l.is_write_vectored(),
            EitherIo::Right(r) => r.is_write_vectored(),
        }
    }
}
