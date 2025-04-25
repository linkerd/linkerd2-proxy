pub use aws_lc_rs::default_provider;
use tokio_rustls::rustls::{
    self,
    crypto::{aws_lc_rs, WebPkiSupportedAlgorithms},
};

pub static TLS_SUPPORTED_CIPHERSUITES: &[rustls::SupportedCipherSuite] =
    &[rustls::crypto::aws_lc_rs::cipher_suite::TLS13_CHACHA20_POLY1305_SHA256];
pub static SUPPORTED_SIG_ALGS: &WebPkiSupportedAlgorithms = &WebPkiSupportedAlgorithms {
    all: &[
        webpki::aws_lc_rs::ECDSA_P256_SHA256,
        webpki::aws_lc_rs::ECDSA_P256_SHA384,
        webpki::aws_lc_rs::ECDSA_P384_SHA256,
        webpki::aws_lc_rs::ECDSA_P384_SHA384,
        webpki::aws_lc_rs::ECDSA_P521_SHA256,
        webpki::aws_lc_rs::ECDSA_P521_SHA384,
        webpki::aws_lc_rs::ECDSA_P521_SHA512,
        webpki::aws_lc_rs::ED25519,
        webpki::aws_lc_rs::RSA_PKCS1_2048_8192_SHA256,
        webpki::aws_lc_rs::RSA_PKCS1_2048_8192_SHA384,
        webpki::aws_lc_rs::RSA_PKCS1_2048_8192_SHA512,
        webpki::aws_lc_rs::RSA_PKCS1_3072_8192_SHA384,
    ],
    mapping: &[
        // Note: for TLS1.2 the curve is not fixed by SignatureScheme. For TLS1.3 it is.
        (
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            &[
                webpki::aws_lc_rs::ECDSA_P384_SHA384,
                webpki::aws_lc_rs::ECDSA_P256_SHA384,
                webpki::aws_lc_rs::ECDSA_P521_SHA384,
            ],
        ),
        (
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            &[
                webpki::aws_lc_rs::ECDSA_P256_SHA256,
                webpki::aws_lc_rs::ECDSA_P384_SHA256,
                webpki::aws_lc_rs::ECDSA_P521_SHA256,
            ],
        ),
        (
            rustls::SignatureScheme::ECDSA_NISTP521_SHA512,
            &[webpki::aws_lc_rs::ECDSA_P521_SHA512],
        ),
        (
            rustls::SignatureScheme::ED25519,
            &[webpki::aws_lc_rs::ED25519],
        ),
        (
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
            &[webpki::aws_lc_rs::RSA_PKCS1_2048_8192_SHA512],
        ),
        (
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            &[webpki::aws_lc_rs::RSA_PKCS1_2048_8192_SHA384],
        ),
        (
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            &[webpki::aws_lc_rs::RSA_PKCS1_2048_8192_SHA256],
        ),
    ],
};
