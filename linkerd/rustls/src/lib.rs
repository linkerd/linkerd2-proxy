pub use crate::crypto::{Config, SIGNATURE_ALG_RUSTLS_SCHEME, SUPPORTED_SIG_ALGS, TLS_VERSIONS};
use std::sync::Arc;
use tokio_rustls::rustls::crypto::CryptoProvider;

mod crypto;

pub fn install_default_provider(config: Config) {
    if CryptoProvider::get_default().is_some() {
        return;
    }

    // Ignore install errors. This is the only place we install a provider, so if we raced with
    // another thread to set the provider it will be functionally the same as this provider.
    let _ = crate::crypto::default_provider(config).install_default();
}

fn install_default_provider_with_defaults() {
    install_default_provider(Config::default());
}

pub fn get_default_provider() -> Arc<CryptoProvider> {
    if let Some(provider) = CryptoProvider::get_default() {
        return Arc::clone(provider);
    }
    install_default_provider_with_defaults();

    Arc::clone(CryptoProvider::get_default().expect("Default crypto provider must be installed"))
}
