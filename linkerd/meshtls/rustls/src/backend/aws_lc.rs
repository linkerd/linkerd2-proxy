pub use aws_lc_rs::default_provider;
use tokio_rustls::rustls::{
    self,
    crypto::{aws_lc_rs, WebPkiSupportedAlgorithms},
};

pub static TLS_SUPPORTED_CIPHERSUITES: &[rustls::SupportedCipherSuite] =
    &[rustls::crypto::aws_lc_rs::cipher_suite::TLS13_CHACHA20_POLY1305_SHA256];
pub static SUPPORTED_SIG_ALGS: &WebPkiSupportedAlgorithms = &WebPkiSupportedAlgorithms {
    all: &[webpki::aws_lc_rs::ECDSA_P256_SHA256],
    mapping: &[(
        crate::creds::params::SIGNATURE_ALG_RUSTLS_SCHEME,
        &[webpki::aws_lc_rs::ECDSA_P256_SHA256],
    )],
};
