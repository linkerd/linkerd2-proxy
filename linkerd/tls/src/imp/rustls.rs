use futures::Future;
use linkerd_identity::{ClientConfig, Name, ServerConfig};
use linkerd_io::{AsyncRead, AsyncWrite, Result};
use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use webpki::DNSNameRef;

#[derive(Clone)]
pub struct TlsConnector(tokio_rustls::TlsConnector);

impl TlsConnector {
    pub fn connect<IO>(&self, domain: Name, stream: IO) -> Connect<IO>
    where
        IO: AsyncRead + AsyncWrite + Unpin,
    {
        let dns = DNSNameRef::try_from_ascii_str(domain.as_ref()).unwrap();
        Connect(self.0.connect(dns, stream))
    }
}

impl From<tokio_rustls::TlsConnector> for TlsConnector {
    fn from(connector: tokio_rustls::TlsConnector) -> Self {
        TlsConnector(connector)
    }
}

impl From<Arc<ClientConfig>> for TlsConnector {
    fn from(conf: Arc<ClientConfig>) -> Self {
        let rustls_config: Arc<rustls::ClientConfig> = conf.as_ref().clone().0.into();
        tokio_rustls::TlsConnector::from(rustls_config).into()
    }
}

pub struct Connect<IO>(tokio_rustls::Connect<IO>);

impl<IO: AsyncRead + AsyncWrite + Unpin> Future for Connect<IO> {
    type Output = Result<client::TlsStream<IO>>;

    #[inline]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx).map(|f| f.map(|s| s.into()))
    }
}

#[derive(Clone)]
pub struct TlsAcceptor(tokio_rustls::TlsAcceptor);

impl TlsAcceptor {
    pub fn accept<IO>(&self, stream: IO) -> Accept<IO>
    where
        IO: AsyncRead + AsyncWrite + Unpin,
    {
        Accept(self.0.accept(stream))
    }
}

impl From<tokio_rustls::TlsAcceptor> for TlsAcceptor {
    fn from(acceptor: tokio_rustls::TlsAcceptor) -> Self {
        TlsAcceptor(acceptor)
    }
}

impl From<Arc<ServerConfig>> for TlsAcceptor {
    fn from(conf: Arc<ServerConfig>) -> Self {
        let rustls_config: Arc<rustls::ServerConfig> = conf.as_ref().clone().0.into();
        tokio_rustls::TlsAcceptor::from(rustls_config).into()
    }
}

pub mod client {
    use std::{
        net::SocketAddr,
        pin::Pin,
        task::{Context, Poll},
    };

    use linkerd_io::{AsyncRead, AsyncWrite, PeerAddr, ReadBuf, Result};
    use rustls::Session;

    use crate::{HasNegotiatedProtocol, NegotiatedProtocolRef};

    #[derive(Debug)]
    pub struct TlsStream<IO>(tokio_rustls::client::TlsStream<IO>);

    impl<IO> TlsStream<IO> {
        pub fn get_alpn_protocol(&self) -> Option<&[u8]> {
            self.0.get_ref().1.get_alpn_protocol()
        }
    }

    impl<IO> From<tokio_rustls::client::TlsStream<IO>> for TlsStream<IO> {
        fn from(stream: tokio_rustls::client::TlsStream<IO>) -> Self {
            TlsStream(stream)
        }
    }

    impl<IO: PeerAddr> PeerAddr for TlsStream<IO> {
        fn peer_addr(&self) -> Result<SocketAddr> {
            self.0.get_ref().0.peer_addr()
        }
    }

    impl<IO> HasNegotiatedProtocol for TlsStream<IO> {
        #[inline]
        fn negotiated_protocol(&self) -> Option<NegotiatedProtocolRef<'_>> {
            self.0
                .get_ref()
                .1
                .get_alpn_protocol()
                .map(NegotiatedProtocolRef)
        }
    }

    impl<IO> AsyncRead for TlsStream<IO>
    where
        IO: AsyncRead + AsyncWrite + Unpin,
    {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<Result<()>> {
            Pin::new(&mut self.0).poll_read(cx, buf)
        }
    }

    impl<IO> AsyncWrite for TlsStream<IO>
    where
        IO: AsyncRead + AsyncWrite + Unpin,
    {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<Result<usize>> {
            Pin::new(&mut self.0).poll_write(cx, buf)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
            Pin::new(&mut self.0).poll_flush(cx)
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
            Pin::new(&mut self.0).poll_shutdown(cx)
        }
    }
}

pub mod server {
    use std::{
        net::SocketAddr,
        pin::Pin,
        task::{Context, Poll},
    };

    use linkerd_dns_name as dns;
    use linkerd_identity::Name;
    use linkerd_io::{AsyncRead, AsyncWrite, PeerAddr, ReadBuf, Result};
    use rustls::Session;

    use crate::{ClientId, HasNegotiatedProtocol, NegotiatedProtocolRef};

    #[derive(Debug)]
    pub struct TlsStream<IO>(tokio_rustls::server::TlsStream<IO>);

    impl<IO> TlsStream<IO> {
        pub fn client_identity(&self) -> Option<ClientId> {
            use webpki::GeneralDNSNameRef;

            let (_io, session) = self.0.get_ref();
            let certs = session.get_peer_certificates()?;
            let c = certs.first().map(rustls::Certificate::as_ref)?;
            let end_cert = webpki::EndEntityCert::from(c).ok()?;
            let dns_names = end_cert.dns_names().ok()?;

            match dns_names.first()? {
                GeneralDNSNameRef::DNSName(n) => {
                    Some(ClientId(Name::from(dns::Name::from(n.to_owned()))))
                }
                GeneralDNSNameRef::Wildcard(_) => {
                    // Wildcards can perhaps be handled in a future path...
                    None
                }
            }
        }
    }

    impl<IO> From<tokio_rustls::server::TlsStream<IO>> for TlsStream<IO> {
        fn from(stream: tokio_rustls::server::TlsStream<IO>) -> Self {
            TlsStream(stream)
        }
    }

    impl<IO: PeerAddr> PeerAddr for TlsStream<IO> {
        fn peer_addr(&self) -> Result<SocketAddr> {
            self.0.get_ref().0.peer_addr()
        }
    }

    impl<IO> AsyncRead for TlsStream<IO>
    where
        IO: AsyncRead + AsyncWrite + Unpin,
    {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<Result<()>> {
            Pin::new(&mut self.0).poll_read(cx, buf)
        }
    }

    impl<IO> AsyncWrite for TlsStream<IO>
    where
        IO: AsyncRead + AsyncWrite + Unpin,
    {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<Result<usize>> {
            Pin::new(&mut self.0).poll_write(cx, buf)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
            Pin::new(&mut self.0).poll_flush(cx)
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
            Pin::new(&mut self.0).poll_shutdown(cx)
        }
    }

    impl<IO> HasNegotiatedProtocol for TlsStream<IO> {
        #[inline]
        fn negotiated_protocol(&self) -> Option<NegotiatedProtocolRef<'_>> {
            self.0
                .get_ref()
                .1
                .get_alpn_protocol()
                .map(|b| NegotiatedProtocolRef(b))
        }
    }
}

pub struct Accept<IO>(tokio_rustls::Accept<IO>);

impl<IO: AsyncRead + AsyncWrite + Unpin> Future for Accept<IO> {
    type Output = Result<server::TlsStream<IO>>;

    #[inline]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx).map(|f| f.map(|s| s.into()))
    }
}
