use crate::{NewClient, Server};
use linkerd_error::Result;
use linkerd_identity::{Credentials, DerX509, Name};

#[cfg(feature = "boring")]
pub use crate::boring;

#[cfg(feature = "rustls")]
pub use crate::rustls;

pub enum Store {
    #[cfg(feature = "boring")]
    Boring(boring::creds::Store),

    #[cfg(feature = "rustls")]
    Rustls(rustls::creds::Store),
}

#[derive(Clone, Debug)]
pub enum Receiver {
    #[cfg(feature = "boring")]
    Boring(boring::creds::Receiver),

    #[cfg(feature = "rustls")]
    Rustls(rustls::creds::Receiver),
}

// === impl Store ===

impl Credentials for Store {
    fn dns_name(&self) -> &Name {
        match self {
            #[cfg(feature = "boring")]
            Self::Boring(store) => store.dns_name(),

            #[cfg(feature = "rustls")]
            Self::Rustls(store) => store.dns_name(),
        }
    }

    fn gen_certificate_signing_request(&mut self) -> DerX509 {
        match self {
            #[cfg(feature = "boring")]
            Self::Boring(store) => store.gen_certificate_signing_request(),

            #[cfg(feature = "rustls")]
            Self::Rustls(store) => store.gen_certificate_signing_request(),
        }
    }

    fn set_certificate(
        &mut self,
        leaf: DerX509,
        chain: Vec<DerX509>,
        expiry: std::time::SystemTime,
    ) -> Result<()> {
        match self {
            #[cfg(feature = "boring")]
            Self::Boring(store) => store.set_certificate(leaf, chain, expiry),

            #[cfg(feature = "rustls")]
            Self::Rustls(store) => store.set_certificate(leaf, chain, expiry),
        }
    }
}

// === impl Receiver ===

#[cfg(feature = "boring")]
impl From<boring::creds::Receiver> for Receiver {
    fn from(rx: boring::creds::Receiver) -> Self {
        Self::Boring(rx)
    }
}

#[cfg(feature = "rustls")]
impl From<rustls::creds::Receiver> for Receiver {
    fn from(rx: rustls::creds::Receiver) -> Self {
        Self::Rustls(rx)
    }
}

impl Receiver {
    pub fn name(&self) -> &Name {
        match self {
            #[cfg(feature = "boring")]
            Self::Boring(receiver) => receiver.name(),

            #[cfg(feature = "rustls")]
            Self::Rustls(receiver) => receiver.name(),
        }
    }

    pub fn new_client(&self) -> NewClient {
        match self {
            #[cfg(feature = "boring")]
            Self::Boring(receiver) => NewClient::Boring(receiver.new_client()),

            #[cfg(feature = "rustls")]
            Self::Rustls(receiver) => NewClient::Rustls(receiver.new_client()),
        }
    }

    pub fn server(&self) -> Server {
        match self {
            #[cfg(feature = "boring")]
            Self::Boring(receiver) => Server::Boring(receiver.server()),

            #[cfg(feature = "rustls")]
            Self::Rustls(receiver) => Server::Rustls(receiver.server()),
        }
    }
}
