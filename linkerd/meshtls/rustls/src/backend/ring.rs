pub use ring::default_provider;
use tokio_rustls::rustls::{
    self,
    crypto::{ring, WebPkiSupportedAlgorithms},
};

pub static TLS_SUPPORTED_CIPHERSUITES: &[rustls::SupportedCipherSuite] =
    &[rustls::crypto::ring::cipher_suite::TLS13_CHACHA20_POLY1305_SHA256];
pub static SUPPORTED_SIG_ALGS: &WebPkiSupportedAlgorithms = &WebPkiSupportedAlgorithms {
    all: &[webpki::ring::ECDSA_P256_SHA256],
    mapping: &[(
        crate::creds::params::SIGNATURE_ALG_RUSTLS_SCHEME,
        &[webpki::ring::ECDSA_P256_SHA256],
    )],
};
