#![allow(irrefutable_let_patterns)]

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
    #[cfg(feature = "rustls")]
    Rustls,

    #[cfg(feature = "boring")]
    Boring,
}

// === impl Mode ===

#[cfg(feature = "rustls")]
impl Default for Mode {
    fn default() -> Self {
        Self::Rustls
    }
}

#[cfg(all(not(feature = "rustls"), feature = "boring"))]
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
        #[cfg(feature = "rustls")]
        if let Self::Rustls = self {
            let (store, receiver) = rustls::creds::watch(identity, roots_pem, key_pkcs8, csr)?;
            return Ok((
                creds::Store::Rustls(store),
                creds::Receiver::Rustls(receiver),
            ));
        }

        #[cfg(feature = "boring")]
        if let Self::Boring = self {
            let (store, receiver) = boring::creds::watch(identity, roots_pem, key_pkcs8, csr)?;
            return Ok((
                creds::Store::Boring(store),
                creds::Receiver::Boring(receiver),
            ));
        }

        unreachable!()
    }
}
