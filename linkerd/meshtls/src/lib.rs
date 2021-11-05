#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]

// Emit a compile-time error if no TLS implementations are enabled. When adding
// new implementations, add their feature flags here!
#[cfg(not(any(feature = "boring", feature = "rustls")))]
compile_error!(
    "at least one of the following TLS implementations must be enabled: 'boring', 'rustls'"
);

mod client;
pub mod creds;
mod server;

pub use self::{
    client::{ClientIo, Connect, ConnectFuture, NewClient},
    server::{Server, ServerIo, TerminateFuture},
};
use linkerd_error::Result;
use linkerd_identity::Name;

#[cfg(feature = "boring")]
pub use linkerd_meshtls_boring as boring;

#[cfg(feature = "rustls")]
pub use linkerd_meshtls_rustls as rustls;

#[derive(Copy, Clone, Debug)]
pub enum Mode {
    #[cfg(feature = "boring")]
    Boring,

    #[cfg(feature = "rustls")]
    Rustls,
}

// === impl Mode ===

#[cfg(feature = "rustls")]
impl Default for Mode {
    fn default() -> Self {
        Self::Rustls
    }
}

// FIXME(ver) We should have a way to opt into boring by configuration when both are enabled.
#[cfg(all(feature = "boring", not(feature = "rustls")))]
impl Default for Mode {
    fn default() -> Self {
        Self::Boring
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
            #[cfg(feature = "boring")]
            Self::Boring => {
                let (store, receiver) = boring::creds::watch(identity, roots_pem, key_pkcs8, csr)?;
                return Ok((
                    creds::Store::Boring(store),
                    creds::Receiver::Boring(receiver),
                ));
            }

            #[cfg(feature = "rustls")]
            Self::Rustls => {
                let (store, receiver) = rustls::creds::watch(identity, roots_pem, key_pkcs8, csr)?;
                Ok((
                    creds::Store::Rustls(store),
                    creds::Receiver::Rustls(receiver),
                ))
            }
        }
    }
}
