use crate::{NewClient, Server};
use linkerd_identity::Name;
use std::sync::Arc;
use tokio::sync::watch;
use tokio_rustls::rustls;

/// Receives TLS config updates to build `NewClient` and `Server` types.
#[derive(Clone)]
pub struct Receiver {
    name: Name,
    client_rx: watch::Receiver<Arc<rustls::ClientConfig>>,
    server_rx: watch::Receiver<Arc<rustls::ServerConfig>>,
}

// === impl Receiver ===

impl Receiver {
    pub(super) fn new(
        name: Name,
        client_rx: watch::Receiver<Arc<rustls::ClientConfig>>,
        server_rx: watch::Receiver<Arc<rustls::ServerConfig>>,
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

impl std::fmt::Debug for Receiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Receiver")
            .field("name", &self.name)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns the simplest default rustls server config.
    ///
    /// This configuration has no server cert, and will fail to accept all
    /// incoming handshakes, but that doesn't matter for these tests, where we
    /// don't actually do any TLS.
    fn empty_server_config() -> rustls::ServerConfig {
        rustls::ServerConfig::builder()
            .with_safe_defaults()
            .with_client_cert_verifier(rustls::server::NoClientAuth::new())
            .with_cert_resolver(Arc::new(rustls::server::ResolvesServerCertUsingSni::new()))
    }

    /// Returns the simplest default rustls client config.
    ///
    /// This configuration will fail to handshake with any TLS servers, because
    /// it doesn't trust any root certificates. However, that doesn't actually
    /// matter for these tests, which don't actually do TLS.
    fn empty_client_config() -> rustls::ClientConfig {
        rustls::ClientConfig::builder()
            .with_safe_defaults()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth()
    }

    #[tokio::test]
    async fn test_server() {
        let init_config = Arc::new(empty_server_config());
        let (server_tx, server_rx) = watch::channel(init_config.clone());
        let (_, client_rx) = watch::channel(Arc::new(empty_client_config()));
        let receiver = Receiver {
            name: "example".parse().unwrap(),
            server_rx,
            client_rx,
        };

        let server = receiver.server();

        assert!(Arc::ptr_eq(&server.config(), &init_config));

        let server_config = Arc::new(empty_server_config());
        server_tx
            .send(server_config.clone())
            .expect("receiver is held");

        assert!(Arc::ptr_eq(&server.config(), &server_config));
    }

    #[tokio::test]
    async fn test_spawn_server_with_alpn() {
        let init_config = Arc::new(empty_server_config());
        let (server_tx, server_rx) = watch::channel(init_config.clone());
        let (_, client_rx) = watch::channel(Arc::new(empty_client_config()));
        let receiver = Receiver {
            name: "example".parse().unwrap(),
            server_rx,
            client_rx,
        };

        let server = receiver
            .server()
            .spawn_with_alpn(vec![b"my.alpn".to_vec()])
            .expect("sender must not be lost");

        let init_sc = server.config();
        assert!(!Arc::ptr_eq(&init_config, &init_sc));
        assert_eq!(init_sc.alpn_protocols, [b"my.alpn"]);

        let update_config = Arc::new(empty_server_config());
        assert!(!Arc::ptr_eq(&update_config, &init_config));
        server_tx
            .send(update_config.clone())
            .expect("receiver is held");

        // Give the update task a chance to run.
        tokio::task::yield_now().await;

        let update_sc = server.config();
        assert!(!Arc::ptr_eq(&update_config, &update_sc));
        assert!(!Arc::ptr_eq(&init_sc, &update_sc));
        assert_eq!(update_sc.alpn_protocols, [b"my.alpn"]);
    }
}
