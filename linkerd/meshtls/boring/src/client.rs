use crate::creds::CredsRx;
use linkerd_identity as id;
use linkerd_io as io;
use linkerd_meshtls_util as util;
use linkerd_stack::{NewService, Service};
use linkerd_tls::{client::AlpnProtocols, ClientTls, NegotiatedProtocolRef, ServerName};
use std::{future::Future, pin::Pin, sync::Arc, task::Context};
use tracing::{debug, trace};

#[derive(Clone)]
pub struct NewClient(CredsRx);

#[derive(Clone)]
pub struct Connect {
    rx: CredsRx,
    alpn: Option<Arc<[Vec<u8>]>>,
    id: id::Id,
    server: ServerName,
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
        Self {
            rx,
            alpn: client_tls.alpn.map(|AlpnProtocols(ps)| ps.into()),
            server: client_tls.server_name,
            id: client_tls.server_id.into(),
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
        let server_name = self.server.clone();
        let server_id = self.id.clone();
        let connector = self
            .rx
            .borrow()
            .connector(self.alpn.as_deref().unwrap_or(&[]));
        Box::pin(async move {
            let config = connector
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
                .configure()
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

            // Establish a TLS connection to the server using the provided
            // `server_name` as an SNI value to the server.
            //
            // Hostname verification is DISABLED, as we do not require that the
            // peer's certificate actually matches the `server_name`. Instead,
            // the `server_id` is used to perform the appropriate form of
            // verification after the session is established.
            let io = tokio_boring::connect(config.verify_hostname(false), server_name.as_str(), io)
                .await
                .map_err(|e| match e.as_io_error() {
                    // TODO(ver) boring should let us take ownership of the error directly.
                    Some(ioe) => io::Error::new(ioe.kind(), ioe.to_string()),
                    // XXX(ver) to use the boring error directly here we have to
                    // constrain the socket on Sync + std::fmt::Debug, which is
                    // a pain.
                    None => io::Error::new(io::ErrorKind::Other, "unexpected TLS handshake error"),
                })?;

            // Servers must present a peer certificate. We extract the x509 cert
            // and verify it manually against the `server_id`.
            let cert = io.ssl().peer_certificate().ok_or_else(|| {
                io::Error::new(io::ErrorKind::Other, "could not extract peer cert")
            })?;
            let cert_der = id::DerX509(cert.to_der()?);
            util::verify_id(&cert_der, &server_id)?;

            debug!(
                tls = io.ssl().version_str(),
                client.cert = ?io.ssl().certificate().and_then(super::fingerprint),
                peer.cert = ?io.ssl().peer_certificate().as_deref().and_then(super::fingerprint),
                alpn = ?io.ssl().selected_alpn_protocol(),
                "Initiated TLS connection"
            );
            trace!(peer.id = %server_id, peer.name = %server_name);
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
