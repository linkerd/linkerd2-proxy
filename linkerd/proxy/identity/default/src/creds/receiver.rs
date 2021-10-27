use crate::{NewClient, Server};
use linkerd_proxy_identity::Name;
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

    #[tokio::test]
    async fn test_server() {
        let init_config = Arc::new(rustls::ServerConfig::new(rustls::NoClientAuth::new()));
        let (server_tx, server_rx) = watch::channel(init_config.clone());
        let (_, client_rx) = watch::channel(Arc::new(rustls::ClientConfig::new()));
        let receiver = Receiver {
            name: "example".parse().unwrap(),
            server_rx,
            client_rx,
        };

        let server = receiver.server();

        assert!(Arc::ptr_eq(&server.config(), &init_config));

        let server_config = Arc::new(rustls::ServerConfig::new(rustls::NoClientAuth::new()));
        server_tx
            .send(server_config.clone())
            .ok()
            .expect("receiver is held");

        assert!(Arc::ptr_eq(&server.config(), &server_config));
    }

    #[tokio::test]
    async fn test_spawn_server_with_alpn() {
        let init_config = Arc::new(rustls::ServerConfig::new(rustls::NoClientAuth::new()));
        let (server_tx, server_rx) = watch::channel(init_config.clone());
        let (_, client_rx) = watch::channel(Arc::new(rustls::ClientConfig::new()));
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

        let update_config = Arc::new(rustls::ServerConfig::new(rustls::NoClientAuth::new()));
        assert!(!Arc::ptr_eq(&update_config, &init_config));
        server_tx
            .send(update_config.clone())
            .ok()
            .expect("receiver is held");

        // Give the update task a chance to run.
        tokio::task::yield_now().await;

        let update_sc = server.config();
        assert!(!Arc::ptr_eq(&update_config, &update_sc));
        assert!(!Arc::ptr_eq(&init_sc, &update_sc));
        assert_eq!(update_sc.alpn_protocols, [b"my.alpn"]);
    }
}
