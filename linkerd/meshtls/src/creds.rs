use crate::{NewClient, Server};
use linkerd_error::Result;
use linkerd_identity::{Credentials, DerX509, Name};

#[cfg(feature = "boring")]
pub use crate::boring;

#[cfg(feature = "rustls")]
pub use crate::rustls;

pub enum Store {
    #[cfg(feature = "rustls")]
    Rustls(rustls::creds::Store),

    #[cfg(feature = "boring")]
    Boring(boring::creds::Store),
}

#[derive(Clone, Debug)]
pub enum Receiver {
    #[cfg(feature = "rustls")]
    Rustls(rustls::creds::Receiver),

    #[cfg(feature = "boring")]
    Boring(boring::creds::Receiver),
}

// === impl Store ===

impl Credentials for Store {
    fn dns_name(&self) -> &Name {
        #[cfg(feature = "rustls")]
        if let Self::Rustls(store) = self {
            return store.dns_name();
        }

        #[cfg(feature = "boring")]
        if let Self::Boring(store) = self {
            return store.dns_name();
        }

        unreachable!()
    }

    fn gen_certificate_signing_request(&mut self) -> DerX509 {
        #[cfg(feature = "rustls")]
        if let Self::Rustls(store) = self {
            return store.gen_certificate_signing_request();
        }

        #[cfg(feature = "boring")]
        if let Self::Boring(store) = self {
            return store.gen_certificate_signing_request();
        }

        unreachable!()
    }

    fn set_certificate(
        &mut self,
        leaf: DerX509,
        chain: Vec<DerX509>,
        expiry: std::time::SystemTime,
    ) -> Result<()> {
        #[cfg(feature = "rustls")]
        if let Self::Rustls(store) = self {
            return store.set_certificate(leaf, chain, expiry);
        }

        #[cfg(feature = "boring")]
        if let Self::Boring(store) = self {
            return store.set_certificate(leaf, chain, expiry);
        }

        unreachable!()
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
        #[cfg(feature = "rustls")]
        if let Self::Rustls(receiver) = self {
            return receiver.name();
        }

        #[cfg(feature = "boring")]
        if let Self::Boring(receiver) = self {
            return receiver.name();
        }

        unreachable!()
    }

    pub fn new_client(&self) -> NewClient {
        #[cfg(feature = "rustls")]
        if let Self::Rustls(receiver) = self {
            return NewClient::Rustls(receiver.new_client());
        }

        #[cfg(feature = "boring")]
        if let Self::Boring(receiver) = self {
            return NewClient::Boring(receiver.new_client());
        }

        unreachable!()
    }

    pub fn server(&self) -> Server {
        #[cfg(feature = "rustls")]
        if let Self::Rustls(receiver) = self {
            return Server::Rustls(receiver.server());
        }

        #[cfg(feature = "boring")]
        if let Self::Boring(receiver) = self {
            return Server::Boring(receiver.server());
        }

        unreachable!()
    }
}
