use crate::{NewClient, Terminate};
use linkerd_error::Result;
use linkerd_proxy_identity::Name;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::watch;
use tokio_rustls::rustls;

#[derive(Clone)]
pub struct Receiver {
    name: Name,
    client_rx: watch::Receiver<Arc<rustls::ClientConfig>>,
    server_rx: watch::Receiver<Arc<rustls::ServerConfig>>,
}

#[derive(Debug, Error)]
#[error("credential store lost")]
pub struct LostStore(());

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

    pub fn new_client(&self) -> NewClient {
        NewClient::new(self.client_rx.clone())
    }

    pub fn server(&self) -> Terminate {
        Terminate::new(self.name.clone(), self.server_rx.clone(), None)
    }

    pub fn spawn_server_with_alpn(
        &self,
        alpn_protocols: Vec<Vec<u8>>,
    ) -> Result<Terminate, LostStore> {
        if alpn_protocols.is_empty() {
            return Ok(self.server());
        }

        let mut orig_rx = self.server_rx.clone();

        let mut c = (**orig_rx.borrow_and_update()).clone();
        c.alpn_protocols = alpn_protocols.clone();
        let (tx, rx) = watch::channel(c.into());

        // Spawn a background task that watches the optional server configuration and publishes it
        // as a reliable channel, including any ALPN overrides.
        eprintln!("spawning task");
        let task = tokio::spawn(async move {
            loop {
                eprintln!("Waiting for update");
                if orig_rx.changed().await.is_err() {
                    return;
                }

                let mut c = (*orig_rx.borrow().clone()).clone();
                c.alpn_protocols = alpn_protocols.clone();
                eprintln!("Updating config");
                if tx.send(c.into()).is_err() {
                    return;
                }
            }
        });

        Ok(Terminate::new(self.name.clone(), rx, Some(task)))
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
            .spawn_server_with_alpn(vec![b"my.alpn".to_vec()])
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
