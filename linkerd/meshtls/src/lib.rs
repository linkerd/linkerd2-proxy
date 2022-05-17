#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

//! This crate provides a static interface for the proxy's x509 certificate
//! provisioning and creation of client/server services. It supports the
//! `boring` and `rustls` TLS backends.
//!
//! This crate may be compiled without either implementation, in which case it
//! will fail at runtime.  This enables an implementation to be chosen by the
//! proxy's frontend, so that other crates can depend on this crate without
//! having to pin a TLS implementation. Furthermore, this crate supports both
//! backends simultaneously so it can be compiled with `--all-features`.

mod client;
pub mod creds;
mod server;

pub use self::{
    client::{ClientIo, Connect, ConnectFuture, NewClient},
    server::{Server, ServerIo, TerminateFuture},
};
use linkerd_error::{Error, Result};
use linkerd_identity::Name;
use std::str::FromStr;

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

    #[cfg(not(feature = "__has_any_tls_impls"))]
    NoTls,
}

#[cfg(not(feature = "__has_any_tls_impls"))]
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

#[cfg(not(feature = "__has_any_tls_impls"))]
impl Default for Mode {
    fn default() -> Self {
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
            #[cfg(feature = "boring")]
            Self::Boring => {
                let (store, receiver) = boring::creds::watch(identity, roots_pem, key_pkcs8, csr)?;
                Ok((
                    creds::Store::Boring(store),
                    creds::Receiver::Boring(receiver),
                ))
            }

            #[cfg(feature = "rustls")]
            Self::Rustls => {
                let (store, receiver) = rustls::creds::watch(identity, roots_pem, key_pkcs8, csr)?;
                Ok((
                    creds::Store::Rustls(store),
                    creds::Receiver::Rustls(receiver),
                ))
            }

            #[cfg(not(feature = "__has_any_tls_impls"))]
            _ => no_tls!(identity, roots_pem, key_pkcs8, csr),
        }
    }
}

impl FromStr for Mode {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        #[cfg(feature = "boring")]
        if s.eq_ignore_ascii_case("boring") {
            return Ok(Self::Boring);
        }

        #[cfg(feature = "rustls")]
        if s.eq_ignore_ascii_case("rustls") {
            return Ok(Self::Rustls);
        }

        Err(format!("unknown TLS backend: {}", s).into())
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(feature = "boring")]
            Self::Boring => "boring".fmt(f),

            #[cfg(feature = "rustls")]
            Self::Rustls => "rustls".fmt(f),

            #[cfg(not(feature = "__has_any_tls_impls"))]
            _ => no_tls!(f),
        }
    }
}
