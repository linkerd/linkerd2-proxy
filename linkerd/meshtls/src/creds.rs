use crate::{NewClient, Server};
use linkerd_error::Result;
use linkerd_identity::{Credentials, DerX509, Name};

#[cfg(feature = "rustls")]
pub use crate::rustls;

pub enum Store {
    #[cfg(feature = "rustls")]
    Rustls(rustls::creds::Store),
    NoTls,
}

#[derive(Clone, Debug)]
pub enum Receiver {
    #[cfg(feature = "rustls")]
    Rustls(rustls::creds::Receiver),
    NoTls,
}

// === impl Store ===

impl Credentials for Store {
    fn dns_name(&self) -> &Name {
        match self {
            #[cfg(feature = "rustls")]
            Self::Rustls(store) => store.dns_name(),
            Self::NoTls => unreachable!("compiled with no TLS implementations enabled!"),
        }
    }

    fn gen_certificate_signing_request(&mut self) -> DerX509 {
        match self {
            #[cfg(feature = "rustls")]
            Self::Rustls(store) => store.gen_certificate_signing_request(),
            Self::NoTls => unreachable!("compiled with no TLS implementations enabled!"),
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
            Self::NoTls => {
                let _ = leaf;
                let _ = chain;
                let _ = expiry;
                unreachable!("compiled with no TLS implementations enabled!")
            }
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

            Self::NoTls => unreachable!("compiled with no TLS implementations enabled!"),
        }
    }

    pub fn new_client(&self) -> NewClient {
        match self {
            #[cfg(feature = "rustls")]
            Self::Rustls(receiver) => NewClient::Rustls(receiver.new_client()),
            Self::NoTls => unreachable!("compiled with no TLS implementations enabled!"),
        }
    }

    pub fn server(&self) -> Server {
        match self {
            #[cfg(feature = "rustls")]
            Self::Rustls(receiver) => Server::Rustls(receiver.server()),
            Self::NoTls => unreachable!("compiled with no TLS implementations enabled!"),
        }
    }
}
