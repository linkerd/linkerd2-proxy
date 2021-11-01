use linkerd_error::Result;
use linkerd_identity::LocalId;
use linkerd_io as io;
use linkerd_stack::{Param, Service};
use linkerd_tls::ServerTls;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

#[cfg(feature = "rustls")]
use crate::rustls;

#[derive(Clone)]
pub enum Server {
    #[cfg(feature = "rustls")]
    Rustls(rustls::Server),
}

#[pin_project::pin_project(project = TerminateFutureProj)]
pub enum TerminateFuture<I> {
    #[cfg(feature = "rustls")]
    Rustls(#[pin] rustls::TerminateFuture<I>),
}

#[pin_project::pin_project(project = ServerIoProj)]
#[derive(Debug)]
pub enum ServerIo<I> {
    #[cfg(feature = "rustls")]
    Rustls(#[pin] rustls::ServerIo<I>),
}

// === impl Server ===

impl Param<LocalId> for Server {
    fn param(&self) -> LocalId {
        #[cfg(feature = "rustls")]
        if let Self::Rustls(srv) = self {
            return srv.param();
        }

        unreachable!()
    }
}

impl Server {
    pub fn spawn_with_alpn(self, alpn_protocols: Vec<Vec<u8>>) -> Result<Self> {
        #[cfg(feature = "rustls")]
        if let Self::Rustls(srv) = self {
            return srv
                .spawn_with_alpn(alpn_protocols)
                .map(Self::Rustls)
                .map_err(Into::into);
        }

        unreachable!()
    }
}

impl<I> Service<I> for Server
where
    I: io::AsyncRead + io::AsyncWrite + Send + Sync + Unpin + 'static,
{
    type Response = (ServerTls, ServerIo<I>);
    type Error = io::Error;
    type Future = TerminateFuture<I>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        #[cfg(feature = "rustls")]
        if let Self::Rustls(svc) = self {
            return <rustls::Server as Service<I>>::poll_ready(svc, cx);
        }

        unreachable!()
    }

    #[inline]
    fn call(&mut self, io: I) -> Self::Future {
        #[cfg(feature = "rustls")]
        if let Self::Rustls(svc) = self {
            return TerminateFuture::Rustls(svc.call(io));
        }

        unreachable!()
    }
}

// === impl TerminateFuture ===

impl<I> Future for TerminateFuture<I>
where
    I: io::AsyncRead + io::AsyncWrite + Unpin,
{
    type Output = io::Result<(ServerTls, ServerIo<I>)>;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        #[cfg(feature = "rustls")]
        if let TerminateFutureProj::Rustls(f) = this {
            let res = futures::ready!(f.poll(cx));
            return Poll::Ready(res.map(|(tls, io)| (tls, ServerIo::Rustls(io))));
        }

        unreachable!()
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
        let this = self.project();

        #[cfg(feature = "rustls")]
        if let ServerIoProj::Rustls(io) = this {
            return io.poll_read(cx, buf);
        }

        unreachable!()
    }
}

impl<I: io::AsyncRead + io::AsyncWrite + Unpin> io::AsyncWrite for ServerIo<I> {
    #[inline]
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> io::Poll<()> {
        let this = self.project();

        #[cfg(feature = "rustls")]
        if let ServerIoProj::Rustls(io) = this {
            return io.poll_flush(cx);
        }

        unreachable!()
    }

    #[inline]
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> io::Poll<()> {
        let this = self.project();

        #[cfg(feature = "rustls")]
        if let ServerIoProj::Rustls(io) = this {
            return io.poll_shutdown(cx);
        }

        unreachable!()
    }

    #[inline]
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> io::Poll<usize> {
        let this = self.project();

        #[cfg(feature = "rustls")]
        if let ServerIoProj::Rustls(io) = this {
            return io.poll_write(cx, buf);
        }

        unreachable!()
    }

    #[inline]
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<Result<usize, std::io::Error>> {
        let this = self.project();

        #[cfg(feature = "rustls")]
        if let ServerIoProj::Rustls(io) = this {
            return io.poll_write_vectored(cx, bufs);
        }

        unreachable!()
    }

    #[inline]
    fn is_write_vectored(&self) -> bool {
        #[cfg(feature = "rustls")]
        if let Self::Rustls(io) = self {
            return io.is_write_vectored();
        }

        unreachable!()
    }
}

impl<I: io::PeerAddr> io::PeerAddr for ServerIo<I> {
    #[inline]
    fn peer_addr(&self) -> io::Result<std::net::SocketAddr> {
        #[cfg(feature = "rustls")]
        if let Self::Rustls(io) = self {
            return io.peer_addr();
        }

        unreachable!()
    }
}
