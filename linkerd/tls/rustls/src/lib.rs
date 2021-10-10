#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]

use futures::prelude::*;
use linkerd_io as io;
use linkerd_stack::Service;
use linkerd_tls::{
    ClientId, ClientTls, HasNegotiatedProtocol, NegotiatedProtocol, NegotiatedProtocolRef,
    ServerTls,
};
use std::{pin::Pin, sync::Arc};
pub use tokio_rustls::rustls::*;
use tracing::debug;

#[derive(Clone)]
pub struct Connect {
    client_tls: ClientTls,
    config: Arc<ClientConfig>,
}

#[derive(Clone)]
pub struct Terminate {
    config: Arc<ServerConfig>,
}

pub type ConnectFuture<I> = futures::future::MapOk<
    tokio_rustls::Connect<I>,
    fn(tokio_rustls::client::TlsStream<I>) -> ClientIo<I>,
>;

pub type TerminateFuture<I> = futures::future::MapOk<
    tokio_rustls::Accept<I>,
    fn(tokio_rustls::server::TlsStream<I>) -> (ServerTls, ServerIo<I>),
>;

#[derive(Debug)]
pub struct ClientIo<I>(tokio_rustls::client::TlsStream<I>);

#[derive(Debug)]
pub struct ServerIo<I>(tokio_rustls::server::TlsStream<I>);

// === impl Connect ===

impl Connect {
    pub fn new(client_tls: ClientTls, config: Arc<ClientConfig>) -> Self {
        Self { client_tls, config }
    }
}

impl<I> Service<I> for Connect
where
    I: io::AsyncRead + io::AsyncWrite + Send + Unpin,
{
    type Response = ClientIo<I>;
    type Error = io::Error;
    type Future = ConnectFuture<I>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, io: I) -> Self::Future {
        tokio_rustls::TlsConnector::from(self.config.clone())
            .connect(self.client_tls.server_id.as_webpki(), io)
            .map_ok(ClientIo)
    }
}

// === impl Terminate ===

pub fn terminate<I>(config: Arc<ServerConfig>, io: I) -> TerminateFuture<I>
where
    I: io::AsyncRead + io::AsyncWrite + Send + Unpin,
{
    tokio_rustls::TlsAcceptor::from(config)
        .accept(io)
        .map_ok(|io| {
            // Determine the peer's identity, if it exist.
            let client_id = client_identity(&io);

            let negotiated_protocol = io
                .get_ref()
                .1
                .get_alpn_protocol()
                .map(|b| NegotiatedProtocol(b.into()));

            debug!(client.id = ?client_id, alpn = ?negotiated_protocol, "Accepted TLS connection");
            let tls = ServerTls::Established {
                client_id,
                negotiated_protocol,
            };
            (tls, ServerIo(io))
        })
}

fn client_identity<I>(tls: &tokio_rustls::server::TlsStream<I>) -> Option<ClientId> {
    let (_io, session) = tls.get_ref();
    let certs = session.get_peer_certificates()?;
    let c = certs.first().map(Certificate::as_ref)?;
    let end_cert = webpki::EndEntityCert::from(c).ok()?;
    let dns_names = end_cert.dns_names().ok()?;

    match dns_names.first()? {
        webpki::GeneralDNSNameRef::DNSName(n) => Some(ClientId(linkerd_identity::Name::from(
            linkerd_dns_name::Name::from(n.to_owned()),
        ))),
        webpki::GeneralDNSNameRef::Wildcard(_) => {
            // Wildcards can perhaps be handled in a future path...
            None
        }
    }
}

// === impl ClientIo ===

impl<I: io::AsyncRead + io::AsyncWrite + Unpin> io::AsyncRead for ClientIo<I> {
    #[inline]
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut io::ReadBuf<'_>,
    ) -> io::Poll<()> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl<I: io::AsyncRead + io::AsyncWrite + Unpin> io::AsyncWrite for ClientIo<I> {
    #[inline]
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> io::Poll<()> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    #[inline]
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> io::Poll<()> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }

    #[inline]
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> io::Poll<usize> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    #[inline]
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        Pin::new(&mut self.0).poll_write_vectored(cx, bufs)
    }

    #[inline]
    fn is_write_vectored(&self) -> bool {
        self.0.is_write_vectored()
    }
}

impl<I> HasNegotiatedProtocol for ClientIo<I> {
    #[inline]
    fn negotiated_protocol(&self) -> Option<NegotiatedProtocolRef<'_>> {
        self.0
            .get_ref()
            .1
            .get_alpn_protocol()
            .map(NegotiatedProtocolRef)
    }
}

impl<I: io::PeerAddr> io::PeerAddr for ClientIo<I> {
    #[inline]
    fn peer_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.0.peer_addr()
    }
}

// === impl ServerIo ===

impl<I: io::AsyncRead + io::AsyncWrite + Unpin> io::AsyncRead for ServerIo<I> {
    #[inline]
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut io::ReadBuf<'_>,
    ) -> io::Poll<()> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl<I: io::AsyncRead + io::AsyncWrite + Unpin> io::AsyncWrite for ServerIo<I> {
    #[inline]
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> io::Poll<()> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    #[inline]
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> io::Poll<()> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }

    #[inline]
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> io::Poll<usize> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    #[inline]
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        Pin::new(&mut self.0).poll_write_vectored(cx, bufs)
    }

    #[inline]
    fn is_write_vectored(&self) -> bool {
        self.0.is_write_vectored()
    }
}

impl<I> HasNegotiatedProtocol for ServerIo<I> {
    #[inline]
    fn negotiated_protocol(&self) -> Option<NegotiatedProtocolRef<'_>> {
        self.0
            .get_ref()
            .1
            .get_alpn_protocol()
            .map(NegotiatedProtocolRef)
    }
}

impl<I: io::PeerAddr> io::PeerAddr for ServerIo<I> {
    #[inline]
    fn peer_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.0.peer_addr()
    }
}
