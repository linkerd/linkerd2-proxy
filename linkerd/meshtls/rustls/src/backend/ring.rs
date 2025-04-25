pub use ring::default_provider;
use tokio_rustls::rustls::{
    self,
    crypto::{ring, WebPkiSupportedAlgorithms},
};

pub static TLS_SUPPORTED_CIPHERSUITES: &[rustls::SupportedCipherSuite] =
    &[rustls::crypto::ring::cipher_suite::TLS13_CHACHA20_POLY1305_SHA256];
// A subset of the algorithms supported by rustls+ring, imported from
// https://github.com/rustls/rustls/blob/v/0.23.21/rustls/src/crypto/ring/mod.rs#L107
pub static SUPPORTED_SIG_ALGS: &WebPkiSupportedAlgorithms = &WebPkiSupportedAlgorithms {
    all: &[
        webpki::ring::ECDSA_P256_SHA256,
        webpki::ring::ECDSA_P256_SHA384,
        webpki::ring::ECDSA_P384_SHA256,
        webpki::ring::ECDSA_P384_SHA384,
        webpki::ring::ED25519,
        webpki::ring::RSA_PKCS1_2048_8192_SHA256,
        webpki::ring::RSA_PKCS1_2048_8192_SHA384,
        webpki::ring::RSA_PKCS1_2048_8192_SHA512,
        webpki::ring::RSA_PKCS1_3072_8192_SHA384,
    ],
    mapping: &[
        (
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            &[
                webpki::ring::ECDSA_P384_SHA384,
                webpki::ring::ECDSA_P256_SHA384,
            ],
        ),
        (
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            &[
                webpki::ring::ECDSA_P256_SHA256,
                webpki::ring::ECDSA_P384_SHA256,
            ],
        ),
        (rustls::SignatureScheme::ED25519, &[webpki::ring::ED25519]),
        (
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
            &[webpki::ring::RSA_PKCS1_2048_8192_SHA512],
        ),
        (
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            &[webpki::ring::RSA_PKCS1_2048_8192_SHA384],
        ),
        (
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            &[webpki::ring::RSA_PKCS1_2048_8192_SHA256],
        ),
    ],
};
