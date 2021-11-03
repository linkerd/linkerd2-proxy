use linkerd_io as io;
use linkerd_stack::{NewService, Service};
use linkerd_tls::{ClientTls, HasNegotiatedProtocol, NegotiatedProtocolRef};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

#[cfg(feature = "boring")]
use crate::boring;

#[cfg(feature = "rustls")]
use crate::rustls;

#[derive(Clone, Debug)]
pub enum NewClient {
    #[cfg(feature = "rustls")]
    Rustls(rustls::NewClient),

    #[cfg(feature = "boring")]
    Boring(boring::NewClient),
}

#[derive(Clone)]
pub enum Connect {
    #[cfg(feature = "rustls")]
    Rustls(rustls::Connect),

    #[cfg(feature = "boring")]
    Boring(boring::Connect),
}

#[pin_project::pin_project(project = ConnectFutureProj)]
pub enum ConnectFuture<I> {
    #[cfg(feature = "rustls")]
    Rustls(#[pin] rustls::ConnectFuture<I>),

    #[cfg(feature = "boring")]
    Boring(#[pin] boring::ConnectFuture<I>),
}

#[pin_project::pin_project(project = ClientIoProj)]
#[derive(Debug)]
pub enum ClientIo<I> {
    #[cfg(feature = "rustls")]
    Rustls(#[pin] rustls::ClientIo<I>),

    #[cfg(feature = "boring")]
    Boring(#[pin] boring::ClientIo<I>),
}

// === impl NewClient ===

impl NewService<ClientTls> for NewClient {
    type Service = Connect;

    fn new_service(&self, target: ClientTls) -> Self::Service {
        #[cfg(feature = "rustls")]
        if let Self::Rustls(new_client) = self {
            return Connect::Rustls(new_client.new_service(target));
        }

        #[cfg(feature = "boring")]
        if let Self::Boring(new_client) = self {
            return Connect::Boring(new_client.new_service(target));
        }

        unreachable!()
    }
}

// === impl Connect ===

impl<I> Service<I> for Connect
where
    I: io::AsyncRead + io::AsyncWrite + Send + Sync + Unpin + 'static,
{
    type Response = ClientIo<I>;
    type Error = io::Error;
    type Future = ConnectFuture<I>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        #[cfg(feature = "rustls")]
        if let Self::Rustls(connect) = self {
            return <rustls::Connect as Service<I>>::poll_ready(connect, cx);
        }

        #[cfg(feature = "boring")]
        if let Self::Boring(connect) = self {
            return <boring::Connect as Service<I>>::poll_ready(connect, cx);
        }

        unreachable!()
    }

    #[inline]
    fn call(&mut self, io: I) -> Self::Future {
        #[cfg(feature = "rustls")]
        if let Self::Rustls(connect) = self {
            return ConnectFuture::Rustls(connect.call(io));
        }

        #[cfg(feature = "boring")]
        if let Self::Boring(connect) = self {
            return ConnectFuture::Boring(connect.call(io));
        }

        unreachable!()
    }
}

// === impl ConnectFuture ===

impl<I> Future for ConnectFuture<I>
where
    I: io::AsyncRead + io::AsyncWrite + Unpin,
{
    type Output = io::Result<ClientIo<I>>;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        #[cfg(feature = "rustls")]
        if let ConnectFutureProj::Rustls(f) = this {
            let res = futures::ready!(f.poll(cx));
            return Poll::Ready(res.map(ClientIo::Rustls));
        }

        #[cfg(feature = "boring")]
        if let ConnectFutureProj::Boring(f) = this {
            let res = futures::ready!(f.poll(cx));
            return Poll::Ready(res.map(ClientIo::Boring));
        }

        unreachable!()
    }
}

// === impl ClientIo ===

impl<I: io::AsyncRead + io::AsyncWrite + Unpin> io::AsyncRead for ClientIo<I> {
    #[inline]
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut io::ReadBuf<'_>,
    ) -> io::Poll<()> {
        let this = self.project();

        #[cfg(feature = "rustls")]
        if let ClientIoProj::Rustls(io) = this {
            return io.poll_read(cx, buf);
        }

        #[cfg(feature = "boring")]
        if let ClientIoProj::Boring(io) = this {
            return io.poll_read(cx, buf);
        }

        unreachable!()
    }
}

impl<I: io::AsyncRead + io::AsyncWrite + Unpin> io::AsyncWrite for ClientIo<I> {
    #[inline]
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> io::Poll<()> {
        let this = self.project();

        #[cfg(feature = "rustls")]
        if let ClientIoProj::Rustls(io) = this {
            return io.poll_flush(cx);
        }

        #[cfg(feature = "boring")]
        if let ClientIoProj::Boring(io) = this {
            return io.poll_flush(cx);
        }

        unreachable!()
    }

    #[inline]
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> io::Poll<()> {
        let this = self.project();

        #[cfg(feature = "rustls")]
        if let ClientIoProj::Rustls(io) = this {
            return io.poll_shutdown(cx);
        }

        #[cfg(feature = "boring")]
        if let ClientIoProj::Boring(io) = this {
            return io.poll_shutdown(cx);
        }

        unreachable!()
    }

    #[inline]
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> io::Poll<usize> {
        let this = self.project();

        #[cfg(feature = "rustls")]
        if let ClientIoProj::Rustls(io) = this {
            return io.poll_write(cx, buf);
        }

        #[cfg(feature = "boring")]
        if let ClientIoProj::Boring(io) = this {
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
        if let ClientIoProj::Rustls(io) = this {
            return io.poll_write_vectored(cx, bufs);
        }

        #[cfg(feature = "boring")]
        if let ClientIoProj::Boring(io) = this {
            return io.poll_write_vectored(cx, bufs);
        }

        unreachable!()
    }

    #[inline]
    fn is_write_vectored(&self) -> bool {
        unimplemented!()
    }
}

impl<I> HasNegotiatedProtocol for ClientIo<I> {
    #[inline]
    fn negotiated_protocol(&self) -> Option<NegotiatedProtocolRef<'_>> {
        unimplemented!()
    }
}

impl<I: io::PeerAddr> io::PeerAddr for ClientIo<I> {
    #[inline]
    fn peer_addr(&self) -> io::Result<std::net::SocketAddr> {
        #[cfg(feature = "rustls")]
        if let Self::Rustls(io) = self {
            return io.peer_addr();
        }

        #[cfg(feature = "boring")]
        if let Self::Boring(io) = self {
            return io.peer_addr();
        }

        unreachable!()
    }
}
