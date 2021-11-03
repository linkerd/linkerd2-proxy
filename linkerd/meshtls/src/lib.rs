#![allow(irrefutable_let_patterns)]

use futures::Future;
use linkerd_error::Result;
use linkerd_identity::{Credentials, DerX509, LocalId, Name};
use linkerd_io as io;
use linkerd_stack::{NewService, Param, Service};
use linkerd_tls::{ClientTls, HasNegotiatedProtocol, NegotiatedProtocolRef, ServerTls};
use std::{
    pin::Pin,
    task::{Context, Poll},
};

#[derive(Copy, Clone, Debug)]
pub enum Mode {
    #[cfg(feature = "rustls")]
    Rustls,

    #[cfg(feature = "boring")]
    Boring,
}

#[derive(Clone, Debug)]
pub enum NewClient {
    #[cfg(feature = "rustls")]
    Rustls(linkerd_identity_rustls_meshtls::NewClient),

    #[cfg(feature = "boring")]
    Boring(linkerd_identity_boring_mozilla_intermediate_v5::NewClient),
}

#[derive(Clone)]
pub enum Connect {
    #[cfg(feature = "rustls")]
    Rustls(linkerd_identity_rustls_meshtls::Connect),

    #[cfg(feature = "boring")]
    Boring(linkerd_identity_boring_mozilla_intermediate_v5::Connect),
}

#[pin_project::pin_project(project = ConnectFutureProj)]
pub enum ConnectFuture<I> {
    #[cfg(feature = "rustls")]
    Rustls(#[pin] linkerd_identity_rustls_meshtls::ConnectFuture<I>),

    #[cfg(feature = "boring")]
    Boring(#[pin] linkerd_identity_boring_mozilla_intermediate_v5::ConnectFuture<I>),
}

#[pin_project::pin_project(project = ClientIoProj)]
#[derive(Debug)]
pub enum ClientIo<I> {
    #[cfg(feature = "rustls")]
    Rustls(#[pin] linkerd_identity_rustls_meshtls::ClientIo<I>),

    #[cfg(feature = "boring")]
    Boring(#[pin] linkerd_identity_boring_mozilla_intermediate_v5::ClientIo<I>),
}

#[derive(Clone)]
pub enum Server {
    #[cfg(feature = "rustls")]
    Rustls(linkerd_identity_rustls_meshtls::Server),

    #[cfg(feature = "boring")]
    Boring(linkerd_identity_boring_mozilla_intermediate_v5::Server),
}

#[pin_project::pin_project(project = TerminateFutureProj)]
pub enum TerminateFuture<I> {
    #[cfg(feature = "rustls")]
    Rustls(#[pin] linkerd_identity_rustls_meshtls::TerminateFuture<I>),

    #[cfg(feature = "boring")]
    Boring(#[pin] linkerd_identity_boring_mozilla_intermediate_v5::TerminateFuture<I>),
}

#[pin_project::pin_project(project = ServerIoProj)]
#[derive(Debug)]
pub enum ServerIo<I> {
    #[cfg(feature = "rustls")]
    Rustls(#[pin] linkerd_identity_rustls_meshtls::ServerIo<I>),

    #[cfg(feature = "boring")]
    Boring(#[pin] linkerd_identity_boring_mozilla_intermediate_v5::ServerIo<I>),
}

// === impl Mode ===

#[cfg(feature = "rustls")]
impl Default for Mode {
    fn default() -> Self {
        Self::Rustls
    }
}

#[cfg(all(not(feature = "rustls"), feature = "boring"))]
impl Default for Mode {
    fn default() -> Self {
        Self::Boring
    }
}

impl Mode {
    pub fn watch(
        self,
        identity: Name,
        roots_pem: &str,
        key_pkcs8: &[u8],
        csr: &[u8],
    ) -> Result<(creds::Store, creds::Receiver)> {
        #[cfg(feature = "rustls")]
        if let Self::Rustls = self {
            let (store, receiver) =
                linkerd_identity_rustls_meshtls::creds::watch(identity, roots_pem, key_pkcs8, csr)?;
            return Ok((
                creds::Store::Rustls(store),
                creds::Receiver::Rustls(receiver),
            ));
        }

        #[cfg(feature = "boring")]
        if let Self::Boring = self {
            let (store, receiver) = linkerd_identity_boring_mozilla_intermediate_v5::creds::watch(
                identity, roots_pem, key_pkcs8, csr,
            )?;
            return Ok((
                creds::Store::Boring(store),
                creds::Receiver::Boring(receiver),
            ));
        }

        unreachable!()
    }
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
            return <linkerd_identity_rustls_meshtls::Connect as Service<I>>::poll_ready(
                connect, cx,
            );
        }

        #[cfg(feature = "boring")]
        if let Self::Boring(connect) = self {
            return <linkerd_identity_boring_mozilla_intermediate_v5::Connect as Service<I>>::poll_ready(
                connect,
                cx,
            );
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

// === impl Server ===

impl Param<LocalId> for Server {
    fn param(&self) -> LocalId {
        #[cfg(feature = "rustls")]
        if let Self::Rustls(srv) = self {
            return srv.param();
        }

        #[cfg(feature = "boring")]
        if let Self::Boring(srv) = self {
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

        #[cfg(feature = "boring")]
        if let Self::Boring(srv) = self {
            return srv.spawn_with_alpn(alpn_protocols).map(Self::Boring);
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
            return <linkerd_identity_rustls_meshtls::Server as Service<I>>::poll_ready(svc, cx);
        }

        #[cfg(feature = "boring")]
        if let Self::Boring(svc) = self {
            return <linkerd_identity_boring_mozilla_intermediate_v5::Server as Service<I>>::poll_ready(
                svc,
                cx,
            );
        }

        unreachable!()
    }

    #[inline]
    fn call(&mut self, io: I) -> Self::Future {
        #[cfg(feature = "rustls")]
        if let Self::Rustls(svc) = self {
            return TerminateFuture::Rustls(svc.call(io));
        }

        #[cfg(feature = "boring")]
        if let Self::Boring(svc) = self {
            return TerminateFuture::Boring(svc.call(io));
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

        #[cfg(feature = "boring")]
        if let TerminateFutureProj::Boring(f) = this {
            let res = futures::ready!(f.poll(cx));
            return Poll::Ready(res.map(|(tls, io)| (tls, ServerIo::Boring(io))));
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

        #[cfg(feature = "boring")]
        if let ServerIoProj::Boring(io) = this {
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

        #[cfg(feature = "boring")]
        if let ServerIoProj::Boring(io) = this {
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

        #[cfg(feature = "boring")]
        if let ServerIoProj::Boring(io) = this {
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

        #[cfg(feature = "boring")]
        if let ServerIoProj::Boring(io) = this {
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

        #[cfg(feature = "boring")]
        if let ServerIoProj::Boring(io) = this {
            return io.poll_write_vectored(cx, bufs);
        }

        unreachable!()
    }

    #[inline]
    fn is_write_vectored(&self) -> bool {
        unimplemented!()
    }
}

impl<I> HasNegotiatedProtocol for ServerIo<I> {
    #[inline]
    fn negotiated_protocol(&self) -> Option<NegotiatedProtocolRef<'_>> {
        unimplemented!()
    }
}

impl<I: io::PeerAddr> io::PeerAddr for ServerIo<I> {
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

pub mod creds {
    use super::*;

    pub enum Store {
        #[cfg(feature = "rustls")]
        Rustls(linkerd_identity_rustls_meshtls::creds::Store),

        #[cfg(feature = "boring")]
        Boring(linkerd_identity_boring_mozilla_intermediate_v5::creds::Store),
    }

    #[derive(Clone, Debug)]
    pub enum Receiver {
        #[cfg(feature = "rustls")]
        Rustls(linkerd_identity_rustls_meshtls::creds::Receiver),

        #[cfg(feature = "boring")]
        Boring(linkerd_identity_boring_mozilla_intermediate_v5::creds::Receiver),
    }

    // === impl Store ===

    impl Credentials for Store {
        fn dns_name(&self) -> &Name {
            #[cfg(feature = "rustls")]
            if let Self::Rustls(store) = self {
                return store.dns_name();
            }

            #[cfg(feature = "boring")]
            if let Self::Boring(store) = self {
                return store.dns_name();
            }

            unreachable!()
        }

        fn gen_certificate_signing_request(&mut self) -> DerX509 {
            #[cfg(feature = "rustls")]
            if let Self::Rustls(store) = self {
                return store.gen_certificate_signing_request();
            }

            #[cfg(feature = "boring")]
            if let Self::Boring(store) = self {
                return store.gen_certificate_signing_request();
            }

            unreachable!()
        }

        fn set_certificate(
            &mut self,
            leaf: DerX509,
            chain: Vec<DerX509>,
            expiry: std::time::SystemTime,
        ) -> Result<()> {
            #[cfg(feature = "rustls")]
            if let Self::Rustls(store) = self {
                return store.set_certificate(leaf, chain, expiry);
            }

            #[cfg(feature = "boring")]
            if let Self::Boring(store) = self {
                return store.set_certificate(leaf, chain, expiry);
            }

            unreachable!()
        }
    }

    // === impl Receiver ===

    impl Receiver {
        pub fn name(&self) -> &Name {
            #[cfg(feature = "rustls")]
            if let Self::Rustls(receiver) = self {
                return receiver.name();
            }

            #[cfg(feature = "boring")]
            if let Self::Boring(receiver) = self {
                return receiver.name();
            }

            unreachable!()
        }

        pub fn new_client(&self) -> NewClient {
            #[cfg(feature = "rustls")]
            if let Self::Rustls(receiver) = self {
                return NewClient::Rustls(receiver.new_client());
            }

            #[cfg(feature = "boring")]
            if let Self::Boring(receiver) = self {
                return NewClient::Boring(receiver.new_client());
            }

            unreachable!()
        }

        pub fn server(&self) -> Server {
            #[cfg(feature = "rustls")]
            if let Self::Rustls(receiver) = self {
                return Server::Rustls(receiver.server());
            }

            #[cfg(feature = "boring")]
            if let Self::Boring(receiver) = self {
                return Server::Boring(receiver.server());
            }

            unreachable!()
        }
    }
}
