use super::CredsRx;
use crate::{NewClient, Server};
use linkerd_identity::{Name, TlsName};

#[derive(Clone)]
pub struct Receiver {
    name: Name,
    tls_name: TlsName,
    rx: CredsRx,
}

impl Receiver {
    pub(crate) fn new(name: Name, tls_name: TlsName, rx: CredsRx) -> Self {
        Self { tls_name, name, rx }
    }

    /// Returns the local identity name.
    pub fn tls_name(&self) -> &TlsName {
        &self.tls_name
    }

    /// Returns a `NewClient` that can be used to establish TLS on client connections.
    pub fn new_client(&self) -> NewClient {
        NewClient::new(self.rx.clone())
    }

    /// Returns a `Server` that can be used to terminate TLS on server connections.
    pub fn server(&self) -> Server {
        Server::new(self.name.clone(), self.rx.clone())
    }
}

impl std::fmt::Debug for Receiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Receiver")
            .field("name", &self.name)
            .field("tls_name", &self.tls_name)
            .finish()
    }
}
