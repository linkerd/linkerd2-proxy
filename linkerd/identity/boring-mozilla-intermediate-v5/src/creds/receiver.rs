use crate::{NewClient, Server};
use boring::ssl;
use linkerd_identity::Name;
use tokio::sync::watch;

#[derive(Clone)]
pub struct Receiver {
    name: Name,
    client_rx: watch::Receiver<ssl::SslConnector>,
    server_rx: watch::Receiver<ssl::SslAcceptor>,
}

impl Receiver {
    pub(crate) fn new(
        name: Name,
        client_rx: watch::Receiver<ssl::SslConnector>,
        server_rx: watch::Receiver<ssl::SslAcceptor>,
    ) -> Self {
        Self {
            name,
            client_rx,
            server_rx,
        }
    }

    /// Returns the local identity.
    pub fn name(&self) -> &Name {
        &self.name
    }

    /// Returns a `NewClient` that can be used to establish TLS on client connections.
    pub fn new_client(&self) -> NewClient {
        NewClient::new(self.client_rx.clone())
    }

    /// Returns a `Server` that can be used to terminate TLS on server connections.
    pub fn server(&self) -> Server {
        Server::new(self.name.clone(), self.server_rx.clone())
    }
}
