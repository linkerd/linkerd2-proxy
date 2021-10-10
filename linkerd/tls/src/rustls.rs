use crate::ClientTls;
use linkerd_io as io;
use linkerd_stack::Service;
use std::sync::Arc;
pub use tokio_rustls::rustls::*;

#[derive(Clone)]
pub struct Connect {
    client_tls: ClientTls,
    config: Arc<ClientConfig>,
}

pub type ConnectFuture<I> = tokio_rustls::Connect<I>;

impl Connect {
    pub fn new(client_tls: ClientTls, config: Arc<ClientConfig>) -> Self {
        Self { client_tls, config }
    }
}

impl<I> Service<I> for Connect
where
    I: io::AsyncRead + io::AsyncWrite + Send + Unpin,
{
    type Response = tokio_rustls::client::TlsStream<I>;
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
