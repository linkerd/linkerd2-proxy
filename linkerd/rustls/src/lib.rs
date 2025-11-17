pub use crate::crypto::{SIGNATURE_ALG_RUSTLS_SCHEME, SUPPORTED_SIG_ALGS, TLS_VERSIONS};
use std::sync::Arc;
use tokio_rustls::rustls::crypto::CryptoProvider;

mod crypto;

pub fn install_default_provider() {
    if CryptoProvider::get_default().is_some() {
        return;
    }

    // Ignore install errors. This is the only place we install a provider, so if we raced with
    // another thread to set the provider it will be functionally the same as this provider.
    let _ = crate::crypto::default_provider().install_default();
}

pub fn get_default_provider() -> Arc<CryptoProvider> {
    if let Some(provider) = CryptoProvider::get_default() {
        return Arc::clone(provider);
    }
    install_default_provider();

    Arc::clone(CryptoProvider::get_default().expect("Default crypto provider must be installed"))
}

#[cfg(feature = "test-util")]
pub mod rcgen {
    // TODO(kate): for now, solely work with 0.14.5.
    pub use rcgen_0_14_5::*;
}

pub mod tokio_rustls {
    // TODO(kate): for now, solely work with 0.26.

    #[cfg(feature = "tokio-rustls-0-26")]
    pub use tokio_rustls_0_26::*;
}
