use crate::creds::CredsRx;
use linkerd_identity::Name;
use linkerd_io as io;
use linkerd_stack::{Param, Service};
use linkerd_tls::{ClientId, LocalId, NegotiatedProtocol, ServerTls};
use std::{future::Future, pin::Pin, sync::Arc, task::Context};
use tracing::debug;

#[derive(Clone)]
pub struct Server {
    name: Name,
    rx: CredsRx,
    alpn: Option<Arc<[Vec<u8>]>>,
}

pub type TerminateFuture<I> =
    Pin<Box<dyn Future<Output = io::Result<(ServerTls, ServerIo<I>)>> + Send>>;

#[derive(Debug)]
pub struct ServerIo<I>(tokio_boring::SslStream<I>);

// === impl Server ===

impl Server {
    pub(crate) fn new(name: Name, rx: CredsRx) -> Self {
        Self {
            name,
            rx,
            alpn: None,
        }
    }

    pub fn with_alpn(mut self, alpn_protocols: Vec<Vec<u8>>) -> Self {
        if !alpn_protocols.is_empty() {
            self.alpn = Some(alpn_protocols.into());
        }

        self
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
        // TODO(ver) we should avoid creating a new context for each connection.
        let acceptor = match &self.alpn {
            Some(alpn) => self.rx.borrow().acceptor(alpn),
            None => self.rx.borrow().acceptor(&[]),
        };

        Box::pin(async move {
            let acc = acceptor.map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
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
            let negotiated_protocol = io.negotiated_protocol();

            debug!(
                tls = io.0.ssl().version_str(),
                srv.cert = ?io.0.ssl().certificate().as_deref().and_then(super::fingerprint),
                peer.cert = ?io.0.ssl().peer_certificate().as_deref().and_then(super::fingerprint),
                client.id = ?client_id,
                alpn = ?negotiated_protocol,
                "Accepted TLS connection"
            );
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
    #[inline]
    fn negotiated_protocol(&self) -> Option<NegotiatedProtocol> {
        self.0
            .ssl()
            .selected_alpn_protocol()
            .map(|p| NegotiatedProtocol(p.to_vec()))
    }

    fn client_identity(&self) -> Option<ClientId> {
        let cert = self.0.ssl().peer_certificate().or_else(|| {
            debug!("Connection missing peer certificate");
            None
        })?;
        let sans = cert.subject_alt_names().or_else(|| {
            debug!("Peer certificate missing SANs");
            None
        })?;
        sans.into_iter()
            .filter_map(|san| san.dnsname()?.parse().ok())
            .next()
            .or_else(|| {
                debug!("Peer certificate missing DNS SANs");
                None
            })
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
