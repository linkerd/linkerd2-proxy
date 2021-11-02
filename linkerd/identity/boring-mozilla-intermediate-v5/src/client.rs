use boring::ssl;
use linkerd_error::Result;
use linkerd_identity::Name;
use linkerd_io as io;
use linkerd_stack::{NewService, Service};
use linkerd_tls::{
    client::AlpnProtocols, ClientTls, HasNegotiatedProtocol, NegotiatedProtocolRef, ServerId,
};
use std::{future::Future, pin::Pin};
use tokio::sync::watch;

#[derive(Clone)]
pub struct NewClient(watch::Receiver<ssl::SslConnector>);

#[derive(Clone)]
pub struct Connect {
    server_id: Name,
    connector: ssl::SslConnector,
}

pub type ConnectFuture<I> = Pin<Box<dyn Future<Output = io::Result<ClientIo<I>>> + Send>>;

#[derive(Debug)]
pub struct ClientIo<I>(tokio_boring::SslStream<I>);

// === impl NewClient ===

impl NewClient {
    pub(crate) fn new(rx: watch::Receiver<ssl::SslConnector>) -> Self {
        Self(rx)
    }
}

impl NewService<ClientTls> for NewClient {
    type Service = Connect;

    fn new_service(&self, target: ClientTls) -> Self::Service {
        Connect::new(target, (*self.0.borrow()).clone())
    }
}

// === impl Connect ===

impl Connect {
    pub(crate) fn new(client_tls: ClientTls, connector: ssl::SslConnector) -> Self {
        let ServerId(server_id) = client_tls.server_id;

        if let Some(AlpnProtocols(protocols)) = client_tls.alpn {
            if !protocols.is_empty() {
                todo!("support ALPN")
            }
        }

        Self {
            server_id,
            connector,
        }
    }
}

impl<I> Service<I> for Connect
where
    I: io::AsyncRead + io::AsyncWrite + Send + Sync + Unpin + std::fmt::Debug + 'static,
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
        let conn = self.connector.clone();
        let id = self.server_id.clone();
        Box::pin(async move {
            let config = conn
                .configure()
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            let io = tokio_boring::connect(config, id.as_str(), io)
                .await
                .map_err(|e| match e.as_io_error() {
                    Some(ioe) => io::Error::new(ioe.kind(), ioe.to_string()),
                    None => io::Error::new(io::ErrorKind::Other, e),
                })?;
            Ok(ClientIo(io))
        })
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
