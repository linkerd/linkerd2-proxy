use crate::{ClientId, HasNegotiatedProtocol, NegotiatedProtocolRef};
use linkerd_identity::{ClientConfig, Name, ServerConfig};
use linkerd_io::{self as io, AsyncRead, AsyncWrite, PeerAddr, ReadBuf, Result};
use std::net::SocketAddr;
use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use std::str::FromStr;
use tracing::{debug, trace, warn};

#[cfg(feature = "boring-tls")]
use {
    boring::{
        fips, ssl,
        ssl::{SslAcceptor, SslConnector, SslConnectorBuilder, SslMethod, SslVerifyMode},
        version,
        x509::store::X509StoreBuilder,
    },
    tokio_boring::SslStream,
};
#[cfg(not(feature = "boring-tls"))]
use {
    openssl::{
        fips, ssl,
        ssl::{Ssl, SslAcceptor, SslConnector, SslConnectorBuilder, SslMethod, SslVerifyMode},
        version,
        x509::store::X509StoreBuilder,
    },
    tokio_openssl::SslStream,
};

#[derive(Clone)]
pub struct TlsConnector(ssl::SslConnector);

impl TlsConnector {
    fn new(conf: Arc<ClientConfig>) -> Result<Self> {
        trace!("Setting up ssl connector");
        let mut builder = SslConnector::builder(SslMethod::tls())?;

        match conf.0.key.clone() {
            None => debug!("No private key provided"),
            Some(key) => {
                debug!("Setting private key {:?}", key);
                builder.set_private_key(key.as_ref().0.as_ref())?;
            }
        }

        match conf.0.cert.clone() {
            None => debug!("No certificate provided"),
            Some(cert) => {
                trace!("Setting certificate {:?}", cert);
                builder.set_certificate(cert.cert.as_ref())?;

                for cert in &cert.chain {
                    trace!("Adding extra chain certificate {:?}", cert);
                    builder.add_extra_chain_cert(cert.clone())?;
                }
            }
        }

        builder.set_cert_store(X509StoreBuilder::new()?.build());
        conf.0
            .root_certs
            .objects()
            .iter()
            .map(|x| x.x509().expect("Unable to get x509 certificate").to_owned())
            .for_each(|cert| {
                trace!("Adding Root certificate {:?}", cert);
                builder
                    .cert_store_mut()
                    .add_cert(cert)
                    .expect("unable to add root certificate")
            });

        Ok(builder.into())
    }

    #[cfg(not(feature = "boring-tls"))]
    pub async fn connect<IO>(&self, domain: Name, stream: IO) -> Result<client::TlsStream<IO>>
    where
        IO: AsyncRead + AsyncWrite + Unpin,
    {
        let ssl = self.0.configure()?.into_ssl(domain.as_ref())?;
        let mut s = TlsStream::new(ssl, stream);
        match Pin::new(&mut s.0).connect().await {
            Ok(_) => Ok(s),
            Err(err) => Err(err
                .into_io_error()
                .unwrap_or_else(|e| io::Error::new(io::ErrorKind::Other, e))),
        }
    }
    #[cfg(feature = "boring-tls")]
    pub async fn connect<IO>(&self, domain: Name, stream: IO) -> Result<client::TlsStream<IO>>
    where
        IO: AsyncRead + AsyncWrite + Unpin,
    {
        let conf = self.0.configure()?;
        match tokio_boring::connect(conf, domain.as_ref(), stream).await {
            Ok(ss) => Ok(ss.into()),
            Err(err) => {
                let _ = err.ssl();
                Err(io::Error::new(ErrorKind::Other, "Ble"))
            }
        }
    }
}

impl From<SslConnector> for TlsConnector {
    fn from(connector: SslConnector) -> Self {
        Self(connector)
    }
}

impl From<SslConnectorBuilder> for TlsConnector {
    fn from(builder: SslConnectorBuilder) -> Self {
        builder.build().into()
    }
}

impl From<Arc<ClientConfig>> for TlsConnector {
    fn from(conf: Arc<ClientConfig>) -> Self {
        TlsConnector::new(conf).expect("unable to create TlsConnector")
    }
}

#[derive(Clone)]
pub struct TlsAcceptor(ssl::SslAcceptor);

impl TlsAcceptor {
    fn new(conf: Arc<ServerConfig>) -> Result<Self> {
        debug!("SSL provider version {}", version::version());
        match fips::enable(true) {
            Err(err) => warn!("FIPS mode can not be enabled {}", err),
            _ => debug!("FIPS mode is enabled"),
        }

        let mut builder = SslAcceptor::mozilla_intermediate(SslMethod::tls())?;

        let key = conf.0.key.as_ref().unwrap().as_ref().0.as_ref();
        trace!("Setting private key {:?}", key);
        builder.set_private_key(key)?;

        let cert = conf.0.cert.as_ref().unwrap().as_ref();
        trace!("Setting certificate {:?}", cert);
        builder.set_certificate(cert.cert.as_ref())?;

        for cert in &cert.chain {
            trace!("Adding extra chain certificate {:?}", cert);
            builder.add_extra_chain_cert(cert.clone())?;
        }

        builder.set_cert_store(X509StoreBuilder::new()?.build());
        conf.0
            .root_certs
            .objects()
            .iter()
            .map(|x| x.x509().expect("Unable to get x509 certificate").to_owned())
            .for_each(|cert| {
                trace!("Adding Root certificate {:?}", cert);
                builder
                    .cert_store_mut()
                    .add_cert(cert)
                    .expect("unable to add root certificate")
            });

        builder.set_verify(SslVerifyMode::PEER);
        builder.check_private_key()?;
        Ok(builder.build().into())
    }

    #[cfg(not(feature = "boring-tls"))]
    pub async fn accept<IO>(&self, stream: IO) -> Result<server::TlsStream<IO>>
    where
        IO: AsyncRead + AsyncWrite + Unpin,
    {
        let ssl = Ssl::new(self.0.context())?;
        let mut s = TlsStream::new(ssl, stream);

        match Pin::new(&mut s.0).accept().await {
            Ok(_) => Ok(s),
            Err(err) => Err(err
                .into_io_error()
                .unwrap_or_else(|e| io::Error::new(io::ErrorKind::Other, e))),
        }
    }
    #[cfg(feature = "boring-tls")]
    pub async fn accept<IO>(&self, stream: IO) -> Result<server::TlsStream<IO>>
    where
        IO: AsyncRead + AsyncWrite + Unpin,
    {
        match tokio_boring::accept(&self.0, stream).await {
            Ok(ss) => Ok(ss.into()),
            Err(err) => {
                let _ = err.ssl();
                Err(io::Error::new(ErrorKind::Other, "Ble"))
            }
        }
    }
}

impl From<SslAcceptor> for TlsAcceptor {
    fn from(acceptor: SslAcceptor) -> Self {
        Self(acceptor)
    }
}

impl From<Arc<ServerConfig>> for TlsAcceptor {
    fn from(conf: Arc<ServerConfig>) -> Self {
        TlsAcceptor::new(conf).expect("unable to create TlsAcceptor")
    }
}

#[derive(Debug)]
pub struct TlsStream<IO>(SslStream<IO>);

impl<IO> TlsStream<IO>
where
    IO: AsyncRead + AsyncWrite,
{
    #[cfg(not(feature = "boring-tls"))]
    pub fn new(ssl: Ssl, stream: IO) -> Self {
        Self(SslStream::new(ssl, stream).expect("unable to create ssl stream"))
    }
}

impl<IO> TlsStream<IO> {
    pub fn get_alpn_protocol(&self) -> Option<&[u8]> {
        self.0.ssl().selected_alpn_protocol()
    }

    pub fn client_identity(&self) -> Option<ClientId> {
        let cert = self.0.ssl().peer_certificate()?;
        let peer = cert.subject_alt_names()?.pop()?;

        ClientId::from_str(peer.dnsname()?).ok()
    }
}

impl<IO> From<SslStream<IO>> for TlsStream<IO> {
    fn from(stream: SslStream<IO>) -> Self {
        TlsStream(stream)
    }
}

impl<IO: PeerAddr> PeerAddr for TlsStream<IO> {
    fn peer_addr(&self) -> Result<SocketAddr> {
        self.0.get_ref().peer_addr()
    }
}

impl<IO> HasNegotiatedProtocol for TlsStream<IO> {
    #[inline]
    fn negotiated_protocol(&self) -> Option<NegotiatedProtocolRef<'_>> {
        self.0
            .ssl()
            .selected_alpn_protocol()
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

pub mod client {
    pub use super::TlsStream;
}

pub mod server {
    pub use super::TlsStream;
}
