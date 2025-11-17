use crate::tokio_rustls::rustls::{
    self,
    crypto::{
        aws_lc_rs::{self, default_provider as aws_lc_default_provider},
        CryptoProvider, SupportedKxGroup, WebPkiSupportedAlgorithms,
    },
};

pub fn default_provider() -> CryptoProvider {
    let mut provider = aws_lc_default_provider();
    provider.cipher_suites = TLS_SUPPORTED_CIPHERSUITES.to_vec();
    provider.kx_groups = kx_groups();
    provider.signature_verification_algorithms = *SUPPORTED_SIG_ALGS;
    #[cfg(feature = "rustls-aws-lc-fips")]
    assert!(provider.fips());
    provider
}

// These must be kept in sync:
pub const SIGNATURE_ALG_RUSTLS_SCHEME: rustls::SignatureScheme =
    rustls::SignatureScheme::ECDSA_NISTP256_SHA256;
pub static TLS_VERSIONS: &[&rustls::SupportedProtocolVersion] = &[&rustls::version::TLS13];

#[cfg(not(feature = "rustls-aws-lc-fips"))]
static TLS_SUPPORTED_CIPHERSUITES: &[rustls::SupportedCipherSuite] = &[
    aws_lc_rs::cipher_suite::TLS13_AES_128_GCM_SHA256,
    aws_lc_rs::cipher_suite::TLS13_AES_256_GCM_SHA384,
    aws_lc_rs::cipher_suite::TLS13_CHACHA20_POLY1305_SHA256,
];
// Prefer aes-256-gcm if fips is enabled
#[cfg(feature = "rustls-aws-lc-fips")]
static TLS_SUPPORTED_CIPHERSUITES: &[rustls::SupportedCipherSuite] = &[
    aws_lc_rs::cipher_suite::TLS13_AES_256_GCM_SHA384,
    aws_lc_rs::cipher_suite::TLS13_AES_128_GCM_SHA256,
];

#[cfg(not(feature = "rustls-aws-lc-fips"))]
fn kx_groups() -> Vec<&'static dyn SupportedKxGroup> {
    vec![
        aws_lc_rs::kx_group::X25519MLKEM768,
        aws_lc_rs::kx_group::X25519,
        aws_lc_rs::kx_group::SECP256R1,
        aws_lc_rs::kx_group::SECP384R1,
    ]
}
#[cfg(feature = "rustls-aws-lc-fips")]
fn kx_groups() -> Vec<&'static dyn SupportedKxGroup> {
    vec![
        aws_lc_rs::kx_group::SECP256R1,
        aws_lc_rs::kx_group::SECP384R1,
    ]
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::NamedGroup;

    #[test]
    fn check_default_cipher_suites() {
        let provider = aws_lc_default_provider();

        assert_eq!(
            provider.cipher_suites.as_slice(),
            &[
                aws_lc_rs::cipher_suite::TLS13_AES_256_GCM_SHA384,
                aws_lc_rs::cipher_suite::TLS13_AES_128_GCM_SHA256,
                aws_lc_rs::cipher_suite::TLS13_CHACHA20_POLY1305_SHA256,
            ]
        );
    }

    #[test]
    fn check_default_kx_groups() {
        let provider = aws_lc_default_provider();

        let kx_groups = provider
            .kx_groups
            .iter()
            .map(|g| g.name())
            .collect::<Vec<_>>();

        assert_eq!(
            kx_groups,
            &[
                NamedGroup::X25519,
                NamedGroup::secp256r1,
                NamedGroup::secp384r1,
                NamedGroup::X25519MLKEM768,
            ]
        );
    }

    #[test]
    fn check_default_signature_verification_algorithms() {
        let provider = aws_lc_default_provider();

        let alg_ids = provider
            .signature_verification_algorithms
            .all
            .iter()
            .map(|g| (g.public_key_alg_id(), g.signature_alg_id()))
            .collect::<Vec<_>>();

        let expected_algs = [
            webpki::aws_lc_rs::ECDSA_P256_SHA256,
            webpki::aws_lc_rs::ECDSA_P256_SHA384,
            webpki::aws_lc_rs::ECDSA_P384_SHA256,
            webpki::aws_lc_rs::ECDSA_P384_SHA384,
            webpki::aws_lc_rs::ECDSA_P521_SHA256,
            webpki::aws_lc_rs::ECDSA_P521_SHA384,
            webpki::aws_lc_rs::ECDSA_P521_SHA512,
            webpki::aws_lc_rs::ED25519,
            webpki::aws_lc_rs::RSA_PSS_2048_8192_SHA256_LEGACY_KEY,
            webpki::aws_lc_rs::RSA_PSS_2048_8192_SHA384_LEGACY_KEY,
            webpki::aws_lc_rs::RSA_PSS_2048_8192_SHA512_LEGACY_KEY,
            webpki::aws_lc_rs::RSA_PKCS1_2048_8192_SHA256,
            webpki::aws_lc_rs::RSA_PKCS1_2048_8192_SHA384,
            webpki::aws_lc_rs::RSA_PKCS1_2048_8192_SHA512,
            webpki::aws_lc_rs::RSA_PKCS1_2048_8192_SHA256_ABSENT_PARAMS,
            webpki::aws_lc_rs::RSA_PKCS1_2048_8192_SHA384_ABSENT_PARAMS,
            webpki::aws_lc_rs::RSA_PKCS1_2048_8192_SHA512_ABSENT_PARAMS,
        ];
        let expected_alg_ids = expected_algs
            .iter()
            .map(|alg| (alg.public_key_alg_id(), alg.signature_alg_id()))
            .collect::<Vec<_>>();

        assert_eq!(alg_ids, expected_alg_ids);
    }
}
