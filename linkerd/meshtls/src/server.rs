use linkerd_error::Result;
use linkerd_io as io;
use linkerd_stack::{Param, Service};
use linkerd_tls::{ServerName, ServerTls};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use crate::rustls;

#[derive(Clone)]
pub enum Server {
    Rustls(rustls::Server),
}

#[pin_project::pin_project(project = TerminateFutureProj)]
pub enum TerminateFuture<I> {
    Rustls(#[pin] rustls::TerminateFuture<I>),
}

#[pin_project::pin_project(project = ServerIoProj)]
#[derive(Debug)]
pub enum ServerIo<I> {
    Rustls(#[pin] rustls::ServerIo<I>),
}

// === impl Server ===

impl Param<ServerName> for Server {
    #[inline]
    fn param(&self) -> ServerName {
        match self {
            Self::Rustls(srv) => srv.param(),
        }
    }
}

impl Server {
    pub fn with_alpn(self, alpn_protocols: Vec<Vec<u8>>) -> Result<Self> {
        match self {
            Self::Rustls(srv) => srv
                .spawn_with_alpn(alpn_protocols)
                .map(Self::Rustls)
                .map_err(Into::into),
        }
    }
}

impl<I> Service<I> for Server
where
    I: io::AsyncRead + io::AsyncWrite + Send + Unpin + 'static,
{
    type Response = (ServerTls, ServerIo<I>);
    type Error = io::Error;
    type Future = TerminateFuture<I>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match self {
            Self::Rustls(svc) => <rustls::Server as Service<I>>::poll_ready(svc, cx),
        }
    }

    #[inline]
    fn call(&mut self, io: I) -> Self::Future {
        match self {
            Self::Rustls(svc) => TerminateFuture::Rustls(svc.call(io)),
        }
    }
}

// === impl TerminateFuture ===

impl<I> Future for TerminateFuture<I>
where
    I: io::AsyncRead + io::AsyncWrite + Unpin,
{
    type Output = io::Result<(ServerTls, ServerIo<I>)>;

    #[inline]
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project() {
            TerminateFutureProj::Rustls(f) => {
                let res = futures::ready!(f.poll(cx));
                Poll::Ready(res.map(|(tls, io)| (tls, ServerIo::Rustls(io))))
            }
        }
    }
}

// === impl ServerIo ===

impl<I: io::AsyncRead + io::AsyncWrite + Unpin> io::AsyncRead for ServerIo<I> {
    #[inline]
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut io::ReadBuf<'_>,
    ) -> io::Poll<()> {
        match self.project() {
            ServerIoProj::Rustls(io) => io.poll_read(cx, buf),
        }
    }
}

impl<I: io::AsyncRead + io::AsyncWrite + Unpin> io::AsyncWrite for ServerIo<I> {
    #[inline]
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> io::Poll<()> {
        match self.project() {
            ServerIoProj::Rustls(io) => io.poll_flush(cx),
        }
    }

    #[inline]
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> io::Poll<()> {
        match self.project() {
            ServerIoProj::Rustls(io) => io.poll_shutdown(cx),
        }
    }

    #[inline]
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> io::Poll<usize> {
        match self.project() {
            ServerIoProj::Rustls(io) => io.poll_write(cx, buf),
        }
    }

    #[inline]
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<Result<usize, std::io::Error>> {
        match self.project() {
            ServerIoProj::Rustls(io) => io.poll_write_vectored(cx, bufs),
        }
    }

    #[inline]
    fn is_write_vectored(&self) -> bool {
        match self {
            Self::Rustls(io) => io.is_write_vectored(),
        }
    }
}

impl<I: io::PeerAddr> io::PeerAddr for ServerIo<I> {
    #[inline]
    fn peer_addr(&self) -> io::Result<std::net::SocketAddr> {
        match self {
            Self::Rustls(io) => io.peer_addr(),
        }
    }
}
