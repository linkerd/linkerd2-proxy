#![allow(irrefutable_let_patterns)]

use futures::Future;
use linkerd_error::Result;
pub use linkerd_identity::*;
use linkerd_io as io;
pub use linkerd_proxy_identity_client as client;
use linkerd_stack::{NewService, Service};
use linkerd_tls::{ClientTls, HasNegotiatedProtocol, NegotiatedProtocolRef, ServerTls};
use std::{
    pin::Pin,
    task::{Context, Poll},
};

#[derive(Copy, Clone, Debug)]
pub enum Mode {
    #[cfg(feature = "identity-rustls-meshtls")]
    RustlsMeshtls,

    #[cfg(feature = "identity-boring-mozilla-intermediate-v5")]
    BoringMozillaIntermediateV5,
}

pub enum Store {
    #[cfg(feature = "identity-rustls-meshtls")]
    RustlsMeshtls(linkerd_identity_rustls_meshtls::creds::Store),

    #[cfg(feature = "identity-boring-mozilla-intermediate-v5")]
    BoringMozillaIntermediateV5(linkerd_identity_boring_mozilla_intermediate_v5::creds::Store),
}

#[derive(Clone, Debug)]
pub enum Receiver {
    #[cfg(feature = "identity-rustls-meshtls")]
    RustlsMeshtls(linkerd_identity_rustls_meshtls::creds::Receiver),

    #[cfg(feature = "identity-boring-mozilla-intermediate-v5")]
    BoringMozillaIntermediateV5(linkerd_identity_boring_mozilla_intermediate_v5::creds::Receiver),
}

#[derive(Clone, Debug)]
pub enum NewClient {
    #[cfg(feature = "identity-rustls-meshtls")]
    RustlsMeshtls(linkerd_identity_rustls_meshtls::NewClient),

    #[cfg(feature = "identity-boring-mozilla-intermediate-v5")]
    BoringMozillaIntermediateV5(linkerd_identity_boring_mozilla_intermediate_v5::NewClient),
}

#[derive(Clone)]
pub enum Connect {
    #[cfg(feature = "identity-rustls-meshtls")]
    RustlsMeshtls(linkerd_identity_rustls_meshtls::Connect),

    #[cfg(feature = "identity-boring-mozilla-intermediate-v5")]
    BoringMozillaIntermediateV5(linkerd_identity_boring_mozilla_intermediate_v5::Connect),
}

#[pin_project::pin_project(project = ConnectFutureProj)]
pub enum ConnectFuture<I> {
    #[cfg(feature = "identity-rustls-meshtls")]
    RustlsMeshtls(#[pin] linkerd_identity_rustls_meshtls::ConnectFuture<I>),

    #[cfg(feature = "identity-boring-mozilla-intermediate-v5")]
    BoringMozillaIntermediateV5(
        #[pin] linkerd_identity_boring_mozilla_intermediate_v5::ConnectFuture<I>,
    ),
}

#[pin_project::pin_project(project = ClientIoProj)]
#[derive(Debug)]
pub enum ClientIo<I> {
    #[cfg(feature = "identity-rustls-meshtls")]
    RustlsMeshtls(#[pin] linkerd_identity_rustls_meshtls::ClientIo<I>),

    #[cfg(feature = "identity-boring-mozilla-intermediate-v5")]
    BoringMozillaIntermediateV5(
        #[pin] linkerd_identity_boring_mozilla_intermediate_v5::ClientIo<I>,
    ),
}

#[derive(Clone)]
pub enum Server {
    #[cfg(feature = "identity-rustls-meshtls")]
    RustlsMeshtls(linkerd_identity_rustls_meshtls::Server),

    #[cfg(feature = "identity-boring-mozilla-intermediate-v5")]
    BoringMozillaIntermediateV5(linkerd_identity_boring_mozilla_intermediate_v5::Server),
}

#[pin_project::pin_project(project = TerminateFutureProj)]
pub enum TerminateFuture<I> {
    #[cfg(feature = "identity-rustls-meshtls")]
    RustlsMeshtls(#[pin] linkerd_identity_rustls_meshtls::TerminateFuture<I>),

    #[cfg(feature = "identity-boring-mozilla-intermediate-v5")]
    BoringMozillaIntermediateV5(
        #[pin] linkerd_identity_boring_mozilla_intermediate_v5::TerminateFuture<I>,
    ),
}

#[pin_project::pin_project(project = ServerIoProj)]
#[derive(Debug)]
pub enum ServerIo<I> {
    #[cfg(feature = "identity-rustls-meshtls")]
    RustlsMeshtls(#[pin] linkerd_identity_rustls_meshtls::ServerIo<I>),

    #[cfg(feature = "identity-boring-mozilla-intermediate-v5")]
    BoringMozillaIntermediateV5(
        #[pin] linkerd_identity_boring_mozilla_intermediate_v5::ServerIo<I>,
    ),
}

// === impl Mode ===

impl Mode {
    pub fn watch(
        self,
        identity: Name,
        roots_pem: &str,
        key_pkcs8: &[u8],
        csr: &[u8],
    ) -> Result<(Store, Receiver)> {
        #[cfg(feature = "identity-rustls-meshtls")]
        if let Self::RustlsMeshtls = self {
            let (store, receiver) =
                linkerd_identity_rustls_meshtls::creds::watch(identity, roots_pem, key_pkcs8, csr)?;
            return Ok((
                Store::RustlsMeshtls(store),
                Receiver::RustlsMeshtls(receiver),
            ));
        }

        #[cfg(feature = "identity-boring-mozilla-intermediate-v5")]
        if let Self::BoringMozillaIntermediateV5 = self {
            let (store, receiver) = linkerd_identity_boring_mozilla_intermediate_v5::creds::watch(
                identity, roots_pem, key_pkcs8, csr,
            )?;
            return Ok((
                Store::BoringMozillaIntermediateV5(store),
                Receiver::BoringMozillaIntermediateV5(receiver),
            ));
        }

        unreachable!()
    }
}

// === impl Store ===

impl Credentials for Store {
    fn dns_name(&self) -> &Name {
        #[cfg(feature = "identity-rustls-meshtls")]
        if let Self::RustlsMeshtls(store) = self {
            return store.dns_name();
        }

        #[cfg(feature = "identity-boring-mozilla-intermediate-v5")]
        if let Self::BoringMozillaIntermediateV5(store) = self {
            return store.dns_name();
        }

        unreachable!()
    }

    fn gen_certificate_signing_request(&mut self) -> DerX509 {
        #[cfg(feature = "identity-rustls-meshtls")]
        if let Self::RustlsMeshtls(store) = self {
            return store.gen_certificate_signing_request();
        }

        #[cfg(feature = "identity-boring-mozilla-intermediate-v5")]
        if let Self::BoringMozillaIntermediateV5(store) = self {
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
        #[cfg(feature = "identity-rustls-meshtls")]
        if let Self::RustlsMeshtls(store) = self {
            return store.set_certificate(leaf, chain, expiry);
        }

        #[cfg(feature = "identity-boring-mozilla-intermediate-v5")]
        if let Self::BoringMozillaIntermediateV5(store) = self {
            return store.set_certificate(leaf, chain, expiry);
        }

        unreachable!()
    }
}

// === impl Receiver ===

impl Receiver {
    pub fn name(&self) -> &Name {
        #[cfg(feature = "identity-rustls-meshtls")]
        if let Self::RustlsMeshtls(receiver) = self {
            return receiver.name();
        }

        #[cfg(feature = "identity-boring-mozilla-intermediate-v5")]
        if let Self::BoringMozillaIntermediateV5(receiver) = self {
            return receiver.name();
        }

        unreachable!()
    }

    pub fn new_client(&self) -> NewClient {
        #[cfg(feature = "identity-rustls-meshtls")]
        if let Self::RustlsMeshtls(receiver) = self {
            return NewClient::RustlsMeshtls(receiver.new_client());
        }

        #[cfg(feature = "identity-boring-mozilla-intermediate-v5")]
        if let Self::BoringMozillaIntermediateV5(receiver) = self {
            return NewClient::BoringMozillaIntermediateV5(receiver.new_client());
        }

        unreachable!()
    }

    pub fn server(&self) -> Server {
        #[cfg(feature = "identity-rustls-meshtls")]
        if let Self::RustlsMeshtls(receiver) = self {
            return Server::RustlsMeshtls(receiver.server());
        }

        #[cfg(feature = "identity-boring-mozilla-intermediate-v5")]
        if let Self::BoringMozillaIntermediateV5(receiver) = self {
            return Server::BoringMozillaIntermediateV5(receiver.server());
        }

        unreachable!()
    }
}

// === impl NewClient ===

impl NewService<ClientTls> for NewClient {
    type Service = Connect;

    fn new_service(&self, target: ClientTls) -> Self::Service {
        #[cfg(feature = "identity-rustls-meshtls")]
        if let Self::RustlsMeshtls(new_client) = self {
            return Connect::RustlsMeshtls(new_client.new_service(target));
        }

        #[cfg(feature = "identity-boring-mozilla-intermediate-v5")]
        if let Self::BoringMozillaIntermediateV5(new_client) = self {
            return Connect::BoringMozillaIntermediateV5(new_client.new_service(target));
        }

        unreachable!()
    }
}

// === impl Connect ===

impl<I> Service<I> for Connect
where
    I: io::AsyncRead + io::AsyncWrite + Send + Sync + Unpin + std::fmt::Debug + 'static,
{
    type Response = ClientIo<I>;
    type Error = io::Error;
    type Future = ConnectFuture<I>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        #[cfg(feature = "identity-rustls-meshtls")]
        if let Self::RustlsMeshtls(connect) = self {
            return <linkerd_identity_rustls_meshtls::Connect as Service<I>>::poll_ready(
                connect, cx,
            );
        }

        #[cfg(feature = "identity-boring-mozilla-intermediate-v5")]
        if let Self::BoringMozillaIntermediateV5(connect) = self {
            return <linkerd_identity_boring_mozilla_intermediate_v5::Connect as Service<I>>::poll_ready(
                connect,
                cx,
            );
        }

        unreachable!()
    }

    #[inline]
    fn call(&mut self, io: I) -> Self::Future {
        #[cfg(feature = "identity-rustls-meshtls")]
        if let Self::RustlsMeshtls(connect) = self {
            return ConnectFuture::RustlsMeshtls(connect.call(io));
        }

        #[cfg(feature = "identity-boring-mozilla-intermediate-v5")]
        if let Self::BoringMozillaIntermediateV5(connect) = self {
            return ConnectFuture::BoringMozillaIntermediateV5(connect.call(io));
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

        #[cfg(feature = "identity-rustls-meshtls")]
        if let ConnectFutureProj::RustlsMeshtls(f) = this {
            let res = futures::ready!(f.poll(cx));
            return Poll::Ready(res.map(ClientIo::RustlsMeshtls));
        }

        #[cfg(feature = "identity-boring-mozilla-intermediate-v5")]
        if let ConnectFutureProj::BoringMozillaIntermediateV5(f) = this {
            let res = futures::ready!(f.poll(cx));
            return Poll::Ready(res.map(ClientIo::BoringMozillaIntermediateV5));
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

        #[cfg(feature = "identity-rustls-meshtls")]
        if let ClientIoProj::RustlsMeshtls(io) = this {
            return io.poll_read(cx, buf);
        }

        #[cfg(feature = "identity-boring-mozilla-intermediate-v5")]
        if let ClientIoProj::BoringMozillaIntermediateV5(io) = this {
            return io.poll_read(cx, buf);
        }

        unreachable!()
    }
}

impl<I: io::AsyncRead + io::AsyncWrite + Unpin> io::AsyncWrite for ClientIo<I> {
    #[inline]
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> io::Poll<()> {
        let this = self.project();

        #[cfg(feature = "identity-rustls-meshtls")]
        if let ClientIoProj::RustlsMeshtls(io) = this {
            return io.poll_flush(cx);
        }

        #[cfg(feature = "identity-boring-mozilla-intermediate-v5")]
        if let ClientIoProj::BoringMozillaIntermediateV5(io) = this {
            return io.poll_flush(cx);
        }

        unreachable!()
    }

    #[inline]
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> io::Poll<()> {
        let this = self.project();

        #[cfg(feature = "identity-rustls-meshtls")]
        if let ClientIoProj::RustlsMeshtls(io) = this {
            return io.poll_shutdown(cx);
        }

        #[cfg(feature = "identity-boring-mozilla-intermediate-v5")]
        if let ClientIoProj::BoringMozillaIntermediateV5(io) = this {
            return io.poll_shutdown(cx);
        }

        unreachable!()
    }

    #[inline]
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> io::Poll<usize> {
        let this = self.project();

        #[cfg(feature = "identity-rustls-meshtls")]
        if let ClientIoProj::RustlsMeshtls(io) = this {
            return io.poll_write(cx, buf);
        }

        #[cfg(feature = "identity-boring-mozilla-intermediate-v5")]
        if let ClientIoProj::BoringMozillaIntermediateV5(io) = this {
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

        #[cfg(feature = "identity-rustls-meshtls")]
        if let ClientIoProj::RustlsMeshtls(io) = this {
            return io.poll_write_vectored(cx, bufs);
        }

        #[cfg(feature = "identity-boring-mozilla-intermediate-v5")]
        if let ClientIoProj::BoringMozillaIntermediateV5(io) = this {
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
        #[cfg(feature = "identity-rustls-meshtls")]
        if let Self::RustlsMeshtls(io) = self {
            return io.peer_addr();
        }

        #[cfg(feature = "identity-boring-mozilla-intermediate-v5")]
        if let Self::BoringMozillaIntermediateV5(io) = self {
            return io.peer_addr();
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

        #[cfg(feature = "identity-rustls-meshtls")]
        if let TerminateFutureProj::RustlsMeshtls(f) = this {
            let res = futures::ready!(f.poll(cx));
            return Poll::Ready(res.map(|(tls, io)| (tls, ServerIo::RustlsMeshtls(io))));
        }

        #[cfg(feature = "identity-boring-mozilla-intermediate-v5")]
        if let TerminateFutureProj::BoringMozillaIntermediateV5(f) = this {
            let res = futures::ready!(f.poll(cx));
            return Poll::Ready(
                res.map(|(tls, io)| (tls, ServerIo::BoringMozillaIntermediateV5(io))),
            );
        }

        unreachable!()
    }
}
