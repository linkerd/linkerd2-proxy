use futures::prelude::*;
use linkerd_io as io;
use linkerd_tls::{
    ClientId, HasNegotiatedProtocol, NegotiatedProtocol, NegotiatedProtocolRef, ServerTls,
};
use std::{pin::Pin, sync::Arc};
use tokio_rustls::rustls::{Certificate, ServerConfig, Session};
use tracing::debug;

pub type TerminateFuture<I> = futures::future::MapOk<
    tokio_rustls::Accept<I>,
    fn(tokio_rustls::server::TlsStream<I>) -> (ServerTls, ServerIo<I>),
>;

#[derive(Debug)]
pub struct ServerIo<I>(tokio_rustls::server::TlsStream<I>);

/// Terminates a TLS connection.
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
        webpki::GeneralDNSNameRef::DNSName(n) => {
            let n = linkerd_dns_name::NameRef::try_from_ascii_str((*n).into()).ok()?;
            Some(ClientId(n.to_owned().into()))
        }
        webpki::GeneralDNSNameRef::Wildcard(_) => {
            // Wildcards can perhaps be handled in a future path...
            None
        }
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
        self.0.get_ref().0.peer_addr()
    }
}
