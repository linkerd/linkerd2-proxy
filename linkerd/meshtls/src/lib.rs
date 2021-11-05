#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]

#[cfg(not(feature = "has_any_tls_impls"))]
#[macro_export]
macro_rules! no_tls {
    ($($field:ident),*) => {
        {
            $(
                let _ = $field;
            )*
            unreachable!("compiled without any TLS implementations enabled!");
        }
    };
}

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

    #[cfg(not(feature = "has_any_tls_impls"))]
    NoTls,
}

// === impl Mode ===

impl Default for Mode {
    fn default() -> Self {
        #[cfg(feature = "rustls")]
        return Self::Rustls;

        // This may not be unreachable if no feature flags are enabled.
        #[cfg(not(feature = "has_any_tls_impls"))]
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

            #[cfg(not(feature = "has_any_tls_impls"))]
            _ => no_tls!(identity, roots_pem, key_pkcs8, csr),
        }
    }
}
