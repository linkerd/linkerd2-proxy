use linkerd_error::Result;
use linkerd_io as io;
use linkerd_stack::{Param, Service};
use linkerd_tls::{ServerName, ServerTls};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

#[cfg(feature = "boring")]
use crate::boring;

#[cfg(feature = "rustls")]
use crate::rustls;

#[cfg(not(feature = "__has_any_tls_impls"))]
use std::marker::PhantomData;

#[derive(Clone)]
pub enum Server {
    #[cfg(feature = "boring")]
    Boring(boring::Server),

    #[cfg(feature = "rustls")]
    Rustls(rustls::Server),

    #[cfg(not(feature = "__has_any_tls_impls"))]
    NoTls,
}

#[pin_project::pin_project(project = TerminateFutureProj)]
pub enum TerminateFuture<I> {
    #[cfg(feature = "boring")]
    Boring(#[pin] boring::TerminateFuture<I>),

    #[cfg(feature = "rustls")]
    Rustls(#[pin] rustls::TerminateFuture<I>),

    #[cfg(not(feature = "__has_any_tls_impls"))]
    NoTls(PhantomData<fn(I)>),
}

#[pin_project::pin_project(project = ServerIoProj)]
#[derive(Debug)]
pub enum ServerIo<I> {
    #[cfg(feature = "boring")]
    Boring(#[pin] boring::ServerIo<I>),

    #[cfg(feature = "rustls")]
    Rustls(#[pin] rustls::ServerIo<I>),

    #[cfg(not(feature = "__has_any_tls_impls"))]
    NoTls(PhantomData<fn(I)>),
}

// === impl Server ===

impl Param<ServerName> for Server {
    #[inline]
    fn param(&self) -> ServerName {
        match self {
            #[cfg(feature = "boring")]
            Self::Boring(srv) => srv.param(),

            #[cfg(feature = "rustls")]
            Self::Rustls(srv) => srv.param(),

            #[cfg(not(feature = "__has_any_tls_impls"))]
            _ => crate::no_tls!(),
        }
    }
}

impl Server {
    pub fn with_alpn(self, alpn_protocols: Vec<Vec<u8>>) -> Result<Self> {
        match self {
            #[cfg(feature = "boring")]
            Self::Boring(srv) => Ok(Self::Boring(srv.with_alpn(alpn_protocols))),

            #[cfg(feature = "rustls")]
            Self::Rustls(srv) => srv
                .spawn_with_alpn(alpn_protocols)
                .map(Self::Rustls)
                .map_err(Into::into),

            #[cfg(not(feature = "__has_any_tls_impls"))]
            _ => crate::no_tls!(alpn_protocols),
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
            #[cfg(feature = "boring")]
            Self::Boring(svc) => <boring::Server as Service<I>>::poll_ready(svc, cx),

            #[cfg(feature = "rustls")]
            Self::Rustls(svc) => <rustls::Server as Service<I>>::poll_ready(svc, cx),

            #[cfg(not(feature = "__has_any_tls_impls"))]
            _ => crate::no_tls!(cx),
        }
    }

    #[inline]
    fn call(&mut self, io: I) -> Self::Future {
        match self {
            #[cfg(feature = "boring")]
            Self::Boring(svc) => TerminateFuture::Boring(svc.call(io)),

            #[cfg(feature = "rustls")]
            Self::Rustls(svc) => TerminateFuture::Rustls(svc.call(io)),

            #[cfg(not(feature = "__has_any_tls_impls"))]
            _ => crate::no_tls!(io),
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
            #[cfg(feature = "boring")]
            TerminateFutureProj::Boring(f) => {
                let res = futures::ready!(f.poll(cx));
                Poll::Ready(res.map(|(tls, io)| (tls, ServerIo::Boring(io))))
            }

            #[cfg(feature = "rustls")]
            TerminateFutureProj::Rustls(f) => {
                let res = futures::ready!(f.poll(cx));
                Poll::Ready(res.map(|(tls, io)| (tls, ServerIo::Rustls(io))))
            }

            #[cfg(not(feature = "__has_any_tls_impls"))]
            _ => crate::no_tls!(cx),
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
            #[cfg(feature = "boring")]
            ServerIoProj::Boring(io) => io.poll_read(cx, buf),

            #[cfg(feature = "rustls")]
            ServerIoProj::Rustls(io) => io.poll_read(cx, buf),

            #[cfg(not(feature = "__has_any_tls_impls"))]
            _ => crate::no_tls!(cx, buf),
        }
    }
}

impl<I: io::AsyncRead + io::AsyncWrite + Unpin> io::AsyncWrite for ServerIo<I> {
    #[inline]
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> io::Poll<()> {
        match self.project() {
            #[cfg(feature = "boring")]
            ServerIoProj::Boring(io) => io.poll_flush(cx),

            #[cfg(feature = "rustls")]
            ServerIoProj::Rustls(io) => io.poll_flush(cx),
            #[cfg(not(feature = "__has_any_tls_impls"))]
            _ => crate::no_tls!(cx),
        }
    }

    #[inline]
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> io::Poll<()> {
        match self.project() {
            #[cfg(feature = "boring")]
            ServerIoProj::Boring(io) => io.poll_shutdown(cx),

            #[cfg(feature = "rustls")]
            ServerIoProj::Rustls(io) => io.poll_shutdown(cx),
            #[cfg(not(feature = "__has_any_tls_impls"))]
            _ => crate::no_tls!(cx),
        }
    }

    #[inline]
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> io::Poll<usize> {
        match self.project() {
            #[cfg(feature = "boring")]
            ServerIoProj::Boring(io) => io.poll_write(cx, buf),

            #[cfg(feature = "rustls")]
            ServerIoProj::Rustls(io) => io.poll_write(cx, buf),
            #[cfg(not(feature = "__has_any_tls_impls"))]
            _ => crate::no_tls!(cx, buf),
        }
    }

    #[inline]
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<Result<usize, std::io::Error>> {
        match self.project() {
            #[cfg(feature = "boring")]
            ServerIoProj::Boring(io) => io.poll_write_vectored(cx, bufs),

            #[cfg(feature = "rustls")]
            ServerIoProj::Rustls(io) => io.poll_write_vectored(cx, bufs),
            #[cfg(not(feature = "__has_any_tls_impls"))]
            _ => crate::no_tls!(cx, bufs),
        }
    }

    #[inline]
    fn is_write_vectored(&self) -> bool {
        match self {
            #[cfg(feature = "boring")]
            Self::Boring(io) => io.is_write_vectored(),

            #[cfg(feature = "rustls")]
            Self::Rustls(io) => io.is_write_vectored(),
            #[cfg(not(feature = "__has_any_tls_impls"))]
            _ => crate::no_tls!(),
        }
    }
}

impl<I: io::PeerAddr> io::PeerAddr for ServerIo<I> {
    #[inline]
    fn peer_addr(&self) -> io::Result<std::net::SocketAddr> {
        match self {
            #[cfg(feature = "boring")]
            Self::Boring(io) => io.peer_addr(),

            #[cfg(feature = "rustls")]
            Self::Rustls(io) => io.peer_addr(),
            #[cfg(not(feature = "__has_any_tls_impls"))]
            _ => crate::no_tls!(),
        }
    }
}
