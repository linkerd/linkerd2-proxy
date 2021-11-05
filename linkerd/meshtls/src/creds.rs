use crate::{NewClient, Server};
use linkerd_error::Result;
use linkerd_identity::{Credentials, DerX509, Name};

#[cfg(feature = "rustls")]
pub use crate::rustls;

pub enum Store {
    #[cfg(feature = "rustls")]
    Rustls(rustls::creds::Store),
    #[cfg(not(feature = "has_any_tls_impls"))]
    NoTls,
}

#[derive(Clone, Debug)]
pub enum Receiver {
    #[cfg(feature = "rustls")]
    Rustls(rustls::creds::Receiver),
    #[cfg(not(feature = "has_any_tls_impls"))]
    NoTls,
}

// === impl Store ===

impl Credentials for Store {
    fn dns_name(&self) -> &Name {
        match self {
            #[cfg(feature = "rustls")]
            Self::Rustls(store) => store.dns_name(),
            #[cfg(not(feature = "has_any_tls_impls"))]
            _ => crate::no_tls!(),
        }
    }

    fn gen_certificate_signing_request(&mut self) -> DerX509 {
        match self {
            #[cfg(feature = "rustls")]
            Self::Rustls(store) => store.gen_certificate_signing_request(),
            #[cfg(not(feature = "has_any_tls_impls"))]
            _ => crate::no_tls!(),
        }
    }

    fn set_certificate(
        &mut self,
        leaf: DerX509,
        chain: Vec<DerX509>,
        expiry: std::time::SystemTime,
    ) -> Result<()> {
        match self {
            #[cfg(feature = "rustls")]
            Self::Rustls(store) => store.set_certificate(leaf, chain, expiry),
            #[cfg(not(feature = "has_any_tls_impls"))]
            _ => crate::no_tls!(leaf, chain, expiry),
        }
    }
}

// === impl Receiver ===

#[cfg(feature = "rustls")]
impl From<rustls::creds::Receiver> for Receiver {
    fn from(rx: rustls::creds::Receiver) -> Self {
        Self::Rustls(rx)
    }
}

impl Receiver {
    pub fn name(&self) -> &Name {
        match self {
            #[cfg(feature = "rustls")]
            Self::Rustls(receiver) => receiver.name(),
            #[cfg(not(feature = "has_any_tls_impls"))]
            _ => crate::no_tls!(),
        }
    }

    pub fn new_client(&self) -> NewClient {
        match self {
            #[cfg(feature = "rustls")]
            Self::Rustls(receiver) => NewClient::Rustls(receiver.new_client()),
            #[cfg(not(feature = "has_any_tls_impls"))]
            _ => crate::no_tls!(),
        }
    }

    pub fn server(&self) -> Server {
        match self {
            #[cfg(feature = "rustls")]
            Self::Rustls(receiver) => Server::Rustls(receiver.server()),
            #[cfg(not(feature = "has_any_tls_impls"))]
            _ => crate::no_tls!(),
        }
    }
}
