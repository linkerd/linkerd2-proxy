use super::CredsRx;
use crate::{NewClient, Server};
use linkerd_identity::Name;

#[derive(Clone)]
pub struct Receiver {
    name: Name,
    rx: CredsRx,
}

impl Receiver {
    pub(crate) fn new(name: Name, rx: CredsRx) -> Self {
        Self { name, rx }
    }

    /// Returns the local identity.
    pub fn name(&self) -> &Name {
        &self.name
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
            .finish()
    }
}
