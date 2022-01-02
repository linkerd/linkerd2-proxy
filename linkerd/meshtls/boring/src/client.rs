use crate::creds::CredsRx;
use linkerd_identity::Name;
use linkerd_io as io;
use linkerd_stack::{NewService, Service};
use linkerd_tls::{
    client::AlpnProtocols, ClientTls, NegotiatedProtocolRef, ServerId,
};
use std::{future::Future, pin::Pin, sync::Arc, task::Context};
use tracing::debug;

#[derive(Clone)]
pub struct NewClient(CredsRx);

#[derive(Clone)]
pub struct Connect {
    rx: CredsRx,
    alpn: Option<Arc<[Vec<u8>]>>,
    server_id: Name,
}

pub type ConnectFuture<I> = Pin<Box<dyn Future<Output = io::Result<ClientIo<I>>> + Send>>;

#[derive(Debug)]
pub struct ClientIo<I>(tokio_boring::SslStream<I>);

// === impl NewClient ===

impl NewClient {
    pub(crate) fn new(rx: CredsRx) -> Self {
        Self(rx)
    }
}

impl NewService<ClientTls> for NewClient {
    type Service = Connect;

    fn new_service(&self, target: ClientTls) -> Self::Service {
        Connect::new(target, self.0.clone())
    }
}

impl std::fmt::Debug for NewClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewClient").finish()
    }
}

// === impl Connect ===

impl Connect {
    pub(crate) fn new(client_tls: ClientTls, rx: CredsRx) -> Self {
        let ServerId(server_id) = client_tls.server_id;
        let alpn = client_tls.alpn.map(|AlpnProtocols(ps)| ps.into());
        Self {
            rx,
            alpn,
            server_id,
        }
    }
}

impl<I> Service<I> for Connect
where
    I: io::AsyncRead + io::AsyncWrite + Send + Unpin + 'static,
{
    type Response = ClientIo<I>;
    type Error = io::Error;
    type Future = ConnectFuture<I>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> io::Poll<()> {
        io::Poll::Ready(Ok(()))
    }

    fn call(&mut self, io: I) -> Self::Future {
        let id = self.server_id.clone();
        let connector = self
            .rx
            .borrow()
            .connector(self.alpn.as_deref().unwrap_or(&[]));
        Box::pin(async move {
            let conn = connector.map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            let config = conn
                .configure()
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            let io = tokio_boring::connect(config, id.as_str(), io)
                .await
                .map_err(|e| match e.as_io_error() {
                    // TODO(ver) boring should let us take ownership of the error directly.
                    Some(ioe) => io::Error::new(ioe.kind(), ioe.to_string()),
                    // XXX(ver) to use the boring error directly here we have to constraint the socket on Sync +
                    // std::fmt::Debug, which is a pain.
                    None => io::Error::new(io::ErrorKind::Other, "unexpected TLS handshake error"),
                })?;

            debug!(
                tls = io.ssl().version_str(),
                client.cert = ?io.ssl().certificate().and_then(super::fingerprint),
                peer.cert = ?io.ssl().peer_certificate().as_deref().and_then(super::fingerprint),
                alpn = ?io.ssl().selected_alpn_protocol(),
                "Initiated TLS connection"
            );
            Ok(ClientIo(io))
        })
    }
}

// === impl ClientIo ===

impl<I: io::AsyncRead + io::AsyncWrite + Unpin> io::AsyncRead for ClientIo<I> {
    #[inline]
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut io::ReadBuf<'_>,
    ) -> io::Poll<()> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl<I: io::AsyncRead + io::AsyncWrite + Unpin> io::AsyncWrite for ClientIo<I> {
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

impl<I> ClientIo<I> {
    #[inline]
    pub fn negotiated_protocol(&self) -> Option<NegotiatedProtocolRef<'_>> {
        self.0
            .ssl()
            .selected_alpn_protocol()
            .map(NegotiatedProtocolRef)
    }
}

impl<I: io::PeerAddr> io::PeerAddr for ClientIo<I> {
    #[inline]
    fn peer_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.0.get_ref().peer_addr()
    }
}
