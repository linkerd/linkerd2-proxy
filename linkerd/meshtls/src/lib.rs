#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]

mod client;
pub mod creds;
mod server;

pub use self::{
    client::{ClientIo, Connect, ConnectFuture, NewClient},
    server::{Server, ServerIo, TerminateFuture},
};
use linkerd_error::Result;
use linkerd_identity::Name;

#[cfg(feature = "rustls")]
pub use linkerd_meshtls_rustls as rustls;

#[derive(Copy, Clone, Debug)]
pub enum Mode {
    #[cfg(feature = "rustls")]
    Rustls,
    NoTls,
}

// === impl Mode ===

impl Default for Mode {
    fn default() -> Self {
        #[cfg(feature = "rustls")]
        return Self::Rustls;

        // This may not be unreachable if no feature flags are enabled.
        #[allow(unreachable_code)]
        Self::NoTls
    }
}

impl Mode {
    pub fn watch(
        self,
        identity: Name,
        roots_pem: &str,
        key_pkcs8: &[u8],
        csr: &[u8],
    ) -> Result<(creds::Store, creds::Receiver)> {
        match self {
            #[cfg(feature = "rustls")]
            Self::Rustls => {
                let (store, receiver) = rustls::creds::watch(identity, roots_pem, key_pkcs8, csr)?;
                Ok((
                    creds::Store::Rustls(store),
                    creds::Receiver::Rustls(receiver),
                ))
            }

            Self::NoTls => {
                let _ = identity;
                let _ = roots_pem;
                let _ = key_pkcs8;
                let _ = csr;
                unreachable!("compiled with no TLS implementations enabled!");
            }
        }
    }
}
