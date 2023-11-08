use super::CredsRx;
use crate::{NewClient, Server};
use linkerd_dns_name as dns;
use linkerd_identity as id;

#[derive(Clone)]
pub struct Receiver {
    id: id::Id,
    name: dns::Name,
    rx: CredsRx,
}

impl Receiver {
    pub(crate) fn new(id: id::Id, name: dns::Name, rx: CredsRx) -> Self {
        Self { id, name, rx }
    }

    /// Returns the local identity.
    pub fn local_id(&self) -> &id::Id {
        &self.id
    }

    /// Returns the mTLS Server Name.
    pub fn server_name(&self) -> &dns::Name {
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
