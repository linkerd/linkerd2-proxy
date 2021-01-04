use super::{AsyncRead, AsyncWrite, IoSlice, PeerAddr, Poll, ReadBuf, Result};
use pin_project::pin_project;
use std::{pin::Pin, task::Context};

#[pin_project(project = EitherIoProj)]
#[derive(Debug)]
pub enum EitherIo<L, R> {
    Left(#[pin] L),
    Right(#[pin] R),
}

impl<L: PeerAddr, R: PeerAddr> PeerAddr for EitherIo<L, R> {
    #[inline]
    fn peer_addr(&self) -> Result<std::net::SocketAddr> {
        match self {
            Self::Left(l) => l.peer_addr(),
            Self::Right(r) => r.peer_addr(),
        }
    }
}

impl<L: AsyncRead, R: AsyncRead> AsyncRead for EitherIo<L, R> {
    #[inline]
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context, buf: &mut ReadBuf<'_>) -> Poll<()> {
        match self.project() {
            EitherIoProj::Left(l) => l.poll_read(cx, buf),
            EitherIoProj::Right(r) => r.poll_read(cx, buf),
        }
    }
}

impl<L: AsyncWrite, R: AsyncWrite> AsyncWrite for EitherIo<L, R> {
    #[inline]
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context) -> Poll<()> {
        match self.project() {
            EitherIoProj::Left(l) => l.poll_shutdown(cx),
            EitherIoProj::Right(r) => r.poll_shutdown(cx),
        }
    }

    #[inline]
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<()> {
        match self.project() {
            EitherIoProj::Left(l) => l.poll_flush(cx),
            EitherIoProj::Right(r) => r.poll_flush(cx),
        }
    }

    #[inline]
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<usize> {
        match self.project() {
            EitherIoProj::Left(l) => l.poll_write(cx, buf),
            EitherIoProj::Right(r) => r.poll_write(cx, buf),
        }
    }

    #[inline]
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &[IoSlice<'_>],
    ) -> Poll<usize> {
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
