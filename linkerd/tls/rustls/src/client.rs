use futures::prelude::*;
use linkerd_io as io;
use linkerd_stack::{NewService, Service};
use linkerd_tls::{client::AlpnProtocols, ClientTls, HasNegotiatedProtocol, NegotiatedProtocolRef};
use std::{pin::Pin, sync::Arc};
use tokio::sync::watch;
use tokio_rustls::rustls::{ClientConfig, Session};

#[derive(Clone)]
pub struct NewClient {
    config: watch::Receiver<Arc<ClientConfig>>,
}

#[derive(Clone)]
pub struct Connect {
    server_id: webpki::DNSName,
    config: Arc<ClientConfig>,
}

pub type ConnectFuture<I> = futures::future::MapOk<
    tokio_rustls::Connect<I>,
    fn(tokio_rustls::client::TlsStream<I>) -> ClientIo<I>,
>;

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

        let server_id = webpki::DNSNameRef::try_from_ascii(client_tls.server_id.as_bytes())
            .expect("identity must be a valid DNS name")
            .to_owned();

        Self { server_id, config }
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
            .connect(self.server_id.as_ref(), io)
            .map_ok(ClientIo)
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
        self.0.get_ref().0.peer_addr()
    }
}
