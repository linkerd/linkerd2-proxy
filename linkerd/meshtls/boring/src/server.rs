use boring::ssl;
use linkerd_error::Result;
use linkerd_identity::Name;
use linkerd_io as io;
use linkerd_stack::{Param, Service};
use linkerd_tls::{ClientId, LocalId, NegotiatedProtocolRef, ServerTls};
use std::{future::Future, pin::Pin, task::Context};
use tokio::sync::watch;
use tracing::debug;

#[derive(Clone)]
pub struct Server {
    name: Name,
    rx: watch::Receiver<ssl::SslAcceptor>,
}

pub type TerminateFuture<I> =
    Pin<Box<dyn Future<Output = io::Result<(ServerTls, ServerIo<I>)>> + Send>>;

#[derive(Debug)]
pub struct ServerIo<I>(tokio_boring::SslStream<I>);

// === impl Server ===

impl Server {
    pub(crate) fn new(name: Name, rx: watch::Receiver<ssl::SslAcceptor>) -> Self {
        Self { name, rx }
    }

    pub fn spawn_with_alpn(self, alpn_protocols: Vec<Vec<u8>>) -> Result<Self> {
        if alpn_protocols.is_empty() {
            return Ok(self);
        }

        // todo!("support ALPN")
        tracing::warn!("ALPN not supported");
        Ok(self)
    }
}

impl Param<LocalId> for Server {
    fn param(&self) -> LocalId {
        LocalId(self.name.clone())
    }
}

impl<I> Service<I> for Server
where
    I: io::AsyncRead + io::AsyncWrite + Send + Unpin + 'static,
{
    type Response = (ServerTls, ServerIo<I>);
    type Error = std::io::Error;
    type Future = TerminateFuture<I>;

    #[inline]
    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> io::Poll<()> {
        io::Poll::Ready(Ok(()))
    }

    fn call(&mut self, io: I) -> Self::Future {
        let acc = (*self.rx.borrow()).clone();
        Box::pin(async move {
            let io = tokio_boring::accept(&acc, io)
                .await
                .map(ServerIo)
                .map_err(|e| match e.as_io_error() {
                    Some(ioe) => io::Error::new(ioe.kind(), ioe.to_string()),
                    // XXX(ver) to use the boring error directly here we have to constraint the
                    // socket on Sync + std::fmt::Debug, which is a pain.
                    None => io::Error::new(io::ErrorKind::Other, "unexpected TLS handshake error"),
                })?;

            let client_id = io.client_identity();
            let negotiated_protocol = io.negotiated_protocol_ref().map(|p| p.to_owned());

            debug!(client.id = ?client_id, alpn = ?negotiated_protocol, "Accepted TLS connection");
            let tls = ServerTls::Established {
                client_id,
                negotiated_protocol,
            };
            Ok((tls, io))
        })
    }
}

// === impl ServerIo ===

impl<I> ServerIo<I> {
    fn negotiated_protocol_ref(&self) -> Option<NegotiatedProtocolRef<'_>> {
        self.0
            .ssl()
            .selected_alpn_protocol()
            .map(NegotiatedProtocolRef)
    }

    fn client_identity(&self) -> Option<ClientId> {
        let cert = self.0.ssl().peer_certificate()?;
        let peer = cert.subject_alt_names()?.pop()?;
        peer.dnsname()?.parse().ok()
    }
}

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

impl<I: io::PeerAddr> io::PeerAddr for ServerIo<I> {
    #[inline]
    fn peer_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.0.get_ref().peer_addr()
    }
}
