use boring::ssl;
use linkerd_error::{Error, Result};
use linkerd_identity::Name;
use linkerd_io as io;
use linkerd_stack::{NewService, Service};
use linkerd_tls::client::{AlpnProtocols, ClientTls, ServerId};
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
