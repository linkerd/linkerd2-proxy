mod receiver;
mod store;

pub use self::{receiver::Receiver, store::Store};
use linkerd_error::Result;
use linkerd_identity as id;
use ring::{error::KeyRejected, signature::EcdsaKeyPair};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::watch;
use tokio_rustls::rustls;
use tracing::warn;

#[derive(Debug, Error)]
#[error(transparent)]
pub struct InvalidKey(KeyRejected);

#[derive(Debug, Error)]
#[error("invalid trust roots")]
pub struct InvalidTrustRoots(());

pub fn watch(
    identity: id::Name,
    roots_pem: &str,
    key_pkcs8: &[u8],
    csr: &[u8],
) -> Result<(Store, Receiver)> {
    let mut roots = rustls::RootCertStore::empty();
    let certs = match rustls_pemfile::certs(&mut std::io::Cursor::new(roots_pem)) {
        Err(error) => {
            warn!(%error, "invalid trust anchors file");
            return Err(error.into());
        }
        Ok(certs) if certs.is_empty() => {
            warn!("no valid certs in trust anchors file");
            return Err(InvalidTrustRoots(()).into());
        }
        Ok(certs) => certs,
    };
    let certs: Vec<webpki::TrustAnchor<'_>> = match certs
        .iter()
        .map(|cert| webpki::TrustAnchor::try_from_cert_der(&cert[..]))
        .collect()
    {
        Err(error) => {
            warn!(%error, "invalid trust anchor");
            return Err(error.into());
        }
        Ok(certs) => certs,
    };

    let key = EcdsaKeyPair::from_pkcs8(params::SIGNATURE_ALG_RING_SIGNING, key_pkcs8)
        .map_err(InvalidKey)?;

    // XXX: Rustls's built-in verifiers don't let us tweak things as fully as we'd like (e.g.
    // controlling the set of trusted signature algorithms), but they provide good enough
    // defaults for now.
    // TODO: lock down the verification further.
    let server_cert_verifier = Arc::new(rustls::client::WebPkiVerifier::new(
        roots.clone(),
        None, // no certificate transparency policy
    ));

    let (client_tx, client_rx) = {
        let mut c = rustls::ClientConfig::builder();
        c.root_store = roots.clone();
        c.enable_tickets = false;
        watch::channel(Arc::new(c))
    };
    let (server_tx, server_rx) = watch::channel(Arc::new(rustls::ServerConfig::new(
        rustls::AllowAnyAnonymousOrAuthenticatedClient::new(roots.clone()),
    )));

    let rx = Receiver::new(identity.clone(), client_rx, server_rx);
    let store = Store::new(roots, key, csr, identity, client_tx, server_tx);

    Ok((store, rx))
}

#[cfg(feature = "test-util")]
pub fn for_test(ent: &linkerd_tls_test_util::Entity) -> (Store, Receiver) {
    watch(
        ent.name.parse().expect("name must be valid"),
        std::str::from_utf8(ent.trust_anchors).expect("roots must be PEM"),
        ent.key,
        b"fake CSR",
    )
    .expect("credentials must be valid")
}

#[cfg(feature = "test-util")]
pub fn default_for_test() -> (Store, Receiver) {
    for_test(&linkerd_tls_test_util::FOO_NS1)
}

fn client_config(
    cert_verifier: Arc<dyn rustls::client::ServerCertVerifier>,
    resolver: CertResolver,
) -> rustls::ClientConfig {
    let cfg = rustls::ClientConfig::builder()
        .with_cipher_suites(TLS_SUPPORTED_CIPHERSUITES)
        .with_safe_default_kx_groups()
        .with_protocol_versions(TLS_VERSIONS)
        .expect("client config must be valid")
        // XXX: Rustls's built-in verifiers don't let us tweak things as fully
        // as we'd like (e.g. controlling the set of trusted signature
        // algorithms), but they provide good enough defaults for now.
        // TODO: lock down the verification further.
        //
        // NOTE(eliza): Rustls considers setting a custom server cert
        // verifier to be a "dangerous configuration", but we're doing
        // *exactly* what its builder API does internally. the difference is
        // just that we want to share the verifier with the `Store`, so that
        // it can be used in `Store::validate`, so we have to `Arc` it and
        // pass it in ourselves. this is considered "dangerous" because we
        // could pass in some arbitrary verifier, but we're actually using
        // the same verifier `rustls` makes by default. so, we're not
        // *actually* doing anything untoward here...
        .with_custom_certificate_verifier(cert_verifier)
        .with_client_cert_resolver(Arc::new(resolver));

    // Disable session resumption for the time-being until resumption is
    // more tested.
    cfg.enable_tickets = false;

    cfg
}

mod params {
    use tokio_rustls::rustls;

    // These must be kept in sync:
    pub static SIGNATURE_ALG_RING_SIGNING: &ring::signature::EcdsaSigningAlgorithm =
        &ring::signature::ECDSA_P256_SHA256_ASN1_SIGNING;
    pub const SIGNATURE_ALG_RUSTLS_SCHEME: rustls::SignatureScheme =
        rustls::SignatureScheme::ECDSA_NISTP256_SHA256;
    pub const SIGNATURE_ALG_RUSTLS_ALGORITHM: rustls::internal::msgs::enums::SignatureAlgorithm =
        rustls::internal::msgs::enums::SignatureAlgorithm::ECDSA;
    pub const TLS_VERSIONS: &[&rustls::SupportedProtocolVersion] = &[&rustls::version::TLS13];
    pub static TLS_SUPPORTED_CIPHERSUITES: &[rustls::SupportedCipherSuite] =
        &[rustls::cipher_suite::TLS13_CHACHA20_POLY1305_SHA256];
}
