use std::time::SystemTime;

pub use crate::rustls;
use crate::{NewClient, Server};
use linkerd_dns_name as dns;
use linkerd_error::Result;
use linkerd_identity::{Credentials, DerX509, Id};

pub enum Store {
    Rustls(rustls::creds::Store),
}

#[derive(Clone, Debug)]
pub enum Receiver {
    Rustls(rustls::creds::Receiver),
}

// === impl Store ===

impl Credentials for Store {
    fn set_certificate(
        &mut self,
        leaf: DerX509,
        chain: Vec<DerX509>,
        key: Vec<u8>,
        exp: SystemTime,
    ) -> Result<()> {
        match self {
            Self::Rustls(store) => store.set_certificate(leaf, chain, key, exp),
        }
    }
}

// === impl Receiver ===

impl From<rustls::creds::Receiver> for Receiver {
    fn from(rx: rustls::creds::Receiver) -> Self {
        Self::Rustls(rx)
    }
}

impl Receiver {
    pub fn local_id(&self) -> &Id {
        match self {
            Self::Rustls(receiver) => receiver.local_id(),
        }
    }

    pub fn server_name(&self) -> &dns::Name {
        match self {
            Self::Rustls(receiver) => receiver.server_name(),
        }
    }

    pub fn new_client(&self) -> NewClient {
        match self {
            Self::Rustls(receiver) => NewClient::Rustls(receiver.new_client()),
        }
    }

    pub fn server(&self) -> Server {
        match self {
            Self::Rustls(receiver) => Server::Rustls(receiver.server()),
        }
    }
}
