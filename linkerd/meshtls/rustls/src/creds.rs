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
    let (added, skipped) = roots
        .add_pem_file(&mut std::io::Cursor::new(roots_pem))
        .map_err(InvalidTrustRoots)?;
    if skipped != 0 {
        warn!("Skipped {} invalid trust anchors", skipped);
    }
    if added == 0 {
        return Err("no trust roots loaded".into());
    }

    let key = EcdsaKeyPair::from_pkcs8(params::SIGNATURE_ALG_RING_SIGNING, key_pkcs8)
        .map_err(InvalidKey)?;

    let (client_tx, client_rx) = {
        let mut c = rustls::ClientConfig::new();
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

mod params {
    use tokio_rustls::rustls;

    // These must be kept in sync:
    pub static SIGNATURE_ALG_RING_SIGNING: &ring::signature::EcdsaSigningAlgorithm =
        &ring::signature::ECDSA_P256_SHA256_ASN1_SIGNING;
    pub const SIGNATURE_ALG_RUSTLS_SCHEME: rustls::SignatureScheme =
        rustls::SignatureScheme::ECDSA_NISTP256_SHA256;
    pub const SIGNATURE_ALG_RUSTLS_ALGORITHM: rustls::internal::msgs::enums::SignatureAlgorithm =
        rustls::internal::msgs::enums::SignatureAlgorithm::ECDSA;
    pub const TLS_VERSIONS: &[rustls::ProtocolVersion] = &[rustls::ProtocolVersion::TLSv1_3];
    pub static TLS_SUPPORTED_CIPHERSUITES: [&rustls::SupportedCipherSuite; 1] =
        [&rustls::ciphersuite::TLS13_CHACHA20_POLY1305_SHA256];
}
