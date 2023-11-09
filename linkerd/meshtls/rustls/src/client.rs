use super::creds::verify;
use futures::prelude::*;
use linkerd_identity as id;
use linkerd_io as io;
use linkerd_stack::{NewService, Service};
use linkerd_tls::{client::AlpnProtocols, ClientTls, NegotiatedProtocolRef};
use std::{convert::TryFrom, pin::Pin, sync::Arc, task::Context};
use tokio::sync::watch;
use tokio_rustls::rustls::{self, ClientConfig};

/// A `NewService` that produces `Connect` services from a dynamic TLS configuration.
#[derive(Clone)]
pub struct NewClient {
    config: watch::Receiver<Arc<ClientConfig>>,
}

/// A `Service` that initiates client-side TLS connections.
#[derive(Clone)]
pub struct Connect {
    server_id: id::Id,
    server_name: rustls::ServerName,
    config: Arc<ClientConfig>,
}

pub type ConnectFuture<I> = Pin<Box<dyn Future<Output = io::Result<ClientIo<I>>> + Send>>;

#[derive(Debug)]
pub struct ClientIo<I>(tokio_rustls::client::TlsStream<I>);

// === impl NewClient ===

impl NewClient {
    pub(crate) fn new(config: watch::Receiver<Arc<ClientConfig>>) -> Self {
        Self { config }
    }
}

impl NewService<ClientTls> for NewClient {
    type Service = Connect;

    fn new_service(&self, target: ClientTls) -> Self::Service {
        Connect::new(target, (*self.config.borrow()).clone())
    }
}

impl std::fmt::Debug for NewClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewClient").finish()
    }
}

// === impl Connect ===

impl Connect {
    pub(crate) fn new(client_tls: ClientTls, config: Arc<ClientConfig>) -> Self {
        // If ALPN protocols are configured by the endpoint, we have to clone the entire
        // configuration and set the protocols. If there are no ALPN options, clone the Arc'd base
        // configuration without extra allocation.
        //
        // TODO it would be better to avoid cloning the whole TLS config per-connection, but the
        // Rustls API doesn't give us a lot of options.
        let config = match client_tls.alpn {
            None => config,
            Some(AlpnProtocols(protocols)) => {
                let mut c = (*config).clone();
                c.alpn_protocols = protocols;
                Arc::new(c)
            }
        };

        let server_name = rustls::ServerName::try_from(client_tls.server_name.as_str())
            .expect("identity must be a valid DNS name");

        Self {
            server_id: client_tls.server_id.into(),
            server_name,
            config,
        }
    }
}

fn extract_cert(c: &rustls::ClientConnection) -> io::Result<&rustls::Certificate> {
    match c.peer_certificates().and_then(|certs| certs.first()) {
        Some(leaf_cert) => io::Result::Ok(leaf_cert),
        None => Err(io::Error::new(io::ErrorKind::Other, "missing tls end cert")),
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
        let server_id = self.server_id.clone();
        Box::pin(
            tokio_rustls::TlsConnector::from(self.config.clone())
                // XXX(eliza): it's a bummer that the server name has to be cloned here...
                .connect(self.server_name.clone(), io)
                .map(move |s| {
                    let s = s?;
                    let (_, conn) = s.get_ref();
                    let end_cert = extract_cert(conn)?;
                    verify::verify_id(end_cert, &server_id)?;
                    Ok(ClientIo(s))
                }),
        )
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
            .get_ref()
            .1
            .alpn_protocol()
            .map(NegotiatedProtocolRef)
    }
}

impl<I: io::PeerAddr> io::PeerAddr for ClientIo<I> {
    #[inline]
    fn peer_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.0.get_ref().0.peer_addr()
    }
}
