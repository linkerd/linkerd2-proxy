use crate::{
    ClientId, ClientTls, HasNegotiatedProtocol, NegotiatedProtocol, NegotiatedProtocolRef,
    ServerTls,
};
use futures::prelude::*;
use linkerd_io as io;
use linkerd_stack::Service;
use std::sync::Arc;
pub use tokio_rustls::rustls::*;
pub use tokio_rustls::{client::TlsStream as ClientStream, server::TlsStream as ServerStream};
use tracing::debug;

#[derive(Clone)]
pub struct Connect {
    client_tls: ClientTls,
    config: Arc<ClientConfig>,
}

#[derive(Clone)]
pub struct Terminate {
    config: Arc<ServerConfig>,
}

pub type ConnectFuture<I> = tokio_rustls::Connect<I>;

pub type TerminateFuture<I> = futures::future::MapOk<
    tokio_rustls::Accept<I>,
    fn(ServerStream<I>) -> (ServerTls, ServerStream<I>),
>;

// === impl Connect ===

impl Connect {
    pub fn new(client_tls: ClientTls, config: Arc<ClientConfig>) -> Self {
        Self { client_tls, config }
    }
}

impl<I> Service<I> for Connect
where
    I: io::AsyncRead + io::AsyncWrite + Send + Unpin,
{
    type Response = ClientStream<I>;
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
            .connect(self.client_tls.server_id.as_webpki(), io)
    }
}

// === impl Terminate ===

impl<I> Service<I> for Terminate
where
    I: io::AsyncRead + io::AsyncWrite + Send + Unpin,
{
    type Response = (ServerTls, ServerStream<I>);
    type Error = io::Error;
    type Future = TerminateFuture<I>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, io: I) -> Self::Future {
        tokio_rustls::TlsAcceptor::from(self.config.clone()).accept(io).map_ok(|io| {
            // Determine the peer's identity, if it exist.
            let client_id = Self::client_identity(&io);

            let negotiated_protocol = io
                .get_ref()
                .1
                .get_alpn_protocol()
                .map(|b| NegotiatedProtocol(b.into()));

            debug!(client.id = ?client_id, alpn = ?negotiated_protocol, "Accepted TLS connection");
            let tls = ServerTls::Established {
                client_id,
                negotiated_protocol,
            };
            (tls, io)
        })
    }
}

impl Terminate {
    fn client_identity<I>(tls: &ServerStream<I>) -> Option<ClientId> {
        let (_io, session) = tls.get_ref();
        let certs = session.get_peer_certificates()?;
        let c = certs.first().map(Certificate::as_ref)?;
        let end_cert = webpki::EndEntityCert::from(c).ok()?;
        let dns_names = end_cert.dns_names().ok()?;

        match dns_names.first()? {
            webpki::GeneralDNSNameRef::DNSName(n) => Some(ClientId(linkerd_identity::Name::from(
                linkerd_dns_name::Name::from(n.to_owned()),
            ))),
            webpki::GeneralDNSNameRef::Wildcard(_) => {
                // Wildcards can perhaps be handled in a future path...
                None
            }
        }
    }
}

impl<I> HasNegotiatedProtocol for ClientStream<I> {
    #[inline]
    fn negotiated_protocol(&self) -> Option<NegotiatedProtocolRef<'_>> {
        self.get_ref()
            .1
            .get_alpn_protocol()
            .map(NegotiatedProtocolRef)
    }
}

impl<I> HasNegotiatedProtocol for ServerStream<I> {
    #[inline]
    fn negotiated_protocol(&self) -> Option<NegotiatedProtocolRef<'_>> {
        self.get_ref()
            .1
            .get_alpn_protocol()
            .map(NegotiatedProtocolRef)
    }
}
