use futures::prelude::*;
use linkerd_dns_name as dns;
use linkerd_io as io;
use linkerd_meshtls_verifier as verifier;
use linkerd_stack::{Param, Service};
use linkerd_tls::{ClientId, NegotiatedProtocol, NegotiatedProtocolRef, ServerName, ServerTls};
use std::{pin::Pin, sync::Arc, task::Context};
use thiserror::Error;
use tokio::sync::watch;
use tokio_rustls::rustls::{Certificate, ServerConfig};
use tracing::debug;

/// A Service that terminates TLS connections using a dynamically updated server configuration.
#[derive(Clone)]
pub struct Server {
    name: dns::Name,
    rx: watch::Receiver<Arc<ServerConfig>>,
}

pub type TerminateFuture<I> = futures::future::MapOk<
    tokio_rustls::Accept<I>,
    fn(tokio_rustls::server::TlsStream<I>) -> (ServerTls, ServerIo<I>),
>;

#[derive(Debug)]
pub struct ServerIo<I>(tokio_rustls::server::TlsStream<I>);

#[derive(Debug, Error)]
#[error("credential store lost")]
pub struct LostStore(());

impl Server {
    pub(crate) fn new(name: dns::Name, rx: watch::Receiver<Arc<ServerConfig>>) -> Self {
        Self { name, rx }
    }

    #[cfg(test)]
    pub(crate) fn config(&self) -> Arc<ServerConfig> {
        (*self.rx.borrow()).clone()
    }

    /// Spawns a background task that watches for TLS configuration updates and creates an augmented
    /// configuration with the provided ALPN protocols. The returned server uses this ALPN-aware
    /// configuration.
    pub fn spawn_with_alpn(self, alpn_protocols: Vec<Vec<u8>>) -> Result<Self, LostStore> {
        if alpn_protocols.is_empty() {
            return Ok(self);
        }

        let mut orig_rx = self.rx;

        let mut c = (**orig_rx.borrow_and_update()).clone();
        c.alpn_protocols.clone_from(&alpn_protocols);
        let (tx, rx) = watch::channel(c.into());

        // Spawn a background task that watches the optional server configuration and publishes it
        // as a reliable channel, including any ALPN overrides.
        //
        // The background task completes when the original sender is closed or when all receivers
        // are dropped.
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tx.closed() => {
                        debug!("ALPN TLS config receivers dropped");
                        return;
                    }
                    res = orig_rx.changed() => {
                        if res.is_err() {
                            debug!("TLS config sender closed");
                            return;
                        }
                    }
                }

                let mut c = (*orig_rx.borrow().clone()).clone();
                c.alpn_protocols.clone_from(&alpn_protocols);
                let _ = tx.send(c.into());
            }
        });

        Ok(Self::new(self.name, rx))
    }
}

impl Param<ServerName> for Server {
    fn param(&self) -> ServerName {
        ServerName(self.name.clone())
    }
}

impl<I> Service<I> for Server
where
    I: io::AsyncRead + io::AsyncWrite + Send + Unpin,
{
    type Response = (ServerTls, ServerIo<I>);
    type Error = std::io::Error;
    type Future = TerminateFuture<I>;

    #[inline]
    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> io::Poll<()> {
        io::Poll::Ready(Ok(()))
    }

    #[inline]
    fn call(&mut self, io: I) -> Self::Future {
        tokio_rustls::TlsAcceptor::from((*self.rx.borrow()).clone())
            .accept(io)
            .map_ok(|io| {
                // Determine the peer's identity, if it exist.
                let client_id = client_identity(&io);

                let negotiated_protocol = io
                    .get_ref()
                    .1
                    .alpn_protocol()
                    .map(|b| NegotiatedProtocol(b.into()));

                debug!(client.id = ?client_id, alpn = ?negotiated_protocol, "Accepted TLS connection");
                let tls = ServerTls::Established {
                    client_id,
                    negotiated_protocol,
                };
                (tls, ServerIo(io))
            })
    }
}

fn client_identity<I>(tls: &tokio_rustls::server::TlsStream<I>) -> Option<ClientId> {
    let (_io, session) = tls.get_ref();
    let certs = session.peer_certificates()?;
    let c = certs.first().map(Certificate::as_ref)?;

    verifier::client_identity(c).map(ClientId)
}

// === impl ServerIo ===

impl<I: io::AsyncRead + io::AsyncWrite + Unpin> io::AsyncRead for ServerIo<I> {
    #[inline]
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut io::ReadBuf<'_>,
    ) -> io::Poll<()> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl<I: io::AsyncRead + io::AsyncWrite + Unpin> io::AsyncWrite for ServerIo<I> {
    #[inline]
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> io::Poll<()> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    #[inline]
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> io::Poll<()> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }

    #[inline]
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> io::Poll<usize> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    #[inline]
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> io::Poll<usize> {
        Pin::new(&mut self.0).poll_write_vectored(cx, bufs)
    }

    #[inline]
    fn is_write_vectored(&self) -> bool {
        self.0.is_write_vectored()
    }
}

impl<I> ServerIo<I> {
    #[inline]
    pub fn negotiated_protocol(&self) -> Option<NegotiatedProtocolRef<'_>> {
        self.0
            .get_ref()
            .1
            .alpn_protocol()
            .map(NegotiatedProtocolRef)
    }
}

impl<I: io::PeerAddr> io::PeerAddr for ServerIo<I> {
    #[inline]
    fn peer_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.0.get_ref().0.peer_addr()
    }
}
