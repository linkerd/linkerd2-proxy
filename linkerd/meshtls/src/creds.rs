use crate::{NewClient, Server};
use linkerd_error::Result;
use linkerd_identity::{Credentials, DerX509, TlsName};

#[cfg(feature = "boring")]
pub use crate::boring;

#[cfg(feature = "rustls")]
pub use crate::rustls;

pub enum Store {
    #[cfg(feature = "boring")]
    Boring(boring::creds::Store),

    #[cfg(feature = "rustls")]
    Rustls(rustls::creds::Store),
    #[cfg(not(feature = "__has_any_tls_impls"))]
    NoTls,
}

#[derive(Clone, Debug)]
pub enum Receiver {
    #[cfg(feature = "boring")]
    Boring(boring::creds::Receiver),

    #[cfg(feature = "rustls")]
    Rustls(rustls::creds::Receiver),
    #[cfg(not(feature = "__has_any_tls_impls"))]
    NoTls,
}

// === impl Store ===

impl Credentials for Store {
    fn tls_name(&self) -> &TlsName {
        match self {
            #[cfg(feature = "boring")]
            Self::Boring(store) => store.tls_name(),

            #[cfg(feature = "rustls")]
            Self::Rustls(store) => store.tls_name(),
            #[cfg(not(feature = "__has_any_tls_impls"))]
            _ => crate::no_tls!(),
        }
    }

    fn gen_certificate_signing_request(&mut self) -> DerX509 {
        match self {
            #[cfg(feature = "boring")]
            Self::Boring(store) => store.gen_certificate_signing_request(),

            #[cfg(feature = "rustls")]
            Self::Rustls(store) => store.gen_certificate_signing_request(),
            #[cfg(not(feature = "__has_any_tls_impls"))]
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
            #[cfg(feature = "boring")]
            Self::Boring(store) => store.set_certificate(leaf, chain, expiry),

            #[cfg(feature = "rustls")]
            Self::Rustls(store) => store.set_certificate(leaf, chain, expiry),
            #[cfg(not(feature = "__has_any_tls_impls"))]
            _ => crate::no_tls!(leaf, chain, expiry),
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
    pub fn tls_name(&self) -> &TlsName {
        match self {
            #[cfg(feature = "boring")]
            Self::Boring(receiver) => receiver.tls_name(),

            #[cfg(feature = "rustls")]
            Self::Rustls(receiver) => receiver.tls_name(),
            #[cfg(not(feature = "__has_any_tls_impls"))]
            _ => crate::no_tls!(),
        }
    }

    pub fn new_client(&self) -> NewClient {
        match self {
            #[cfg(feature = "boring")]
            Self::Boring(receiver) => NewClient::Boring(receiver.new_client()),

            #[cfg(feature = "rustls")]
            Self::Rustls(receiver) => NewClient::Rustls(receiver.new_client()),
            #[cfg(not(feature = "__has_any_tls_impls"))]
            _ => crate::no_tls!(),
        }
    }

    pub fn server(&self) -> Server {
        match self {
            #[cfg(feature = "boring")]
            Self::Boring(receiver) => Server::Boring(receiver.server()),

            #[cfg(feature = "rustls")]
            Self::Rustls(receiver) => Server::Rustls(receiver.server()),
            #[cfg(not(feature = "__has_any_tls_impls"))]
            _ => crate::no_tls!(),
        }
    }
}
