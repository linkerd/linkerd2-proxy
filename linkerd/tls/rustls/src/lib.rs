#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]
#![allow(dead_code)]

mod client;
mod server;
#[cfg(feature = "test-util")]
pub mod test_util;

pub use self::{
    client::{ClientIo, Connect, ConnectFuture},
    server::{terminate, ServerIo, TerminateFuture},
};
use linkerd_error::Result;
use linkerd_proxy_identity as id;
pub use ring::error::KeyRejected;
use ring::{rand, signature::EcdsaKeyPair};
use std::{sync::Arc, time::SystemTime};
use thiserror::Error;
pub use tokio_rustls::rustls::*;
use tracing::{debug, warn};

#[derive(Clone)]
pub struct Credentials {
    roots: RootCertStore,
    key: Arc<EcdsaKeyPair>,
    csr: Arc<[u8]>,
    identity: id::Name,
    certificate: Option<Crt>,
}

#[derive(Debug, Error)]
#[error(transparent)]
pub struct InvalidKey(KeyRejected);

#[derive(Debug, Error)]
#[error("invalid trust roots")]
pub struct InvalidTrustRoots(());

#[derive(Clone, Debug)]
struct Crt {
    chain: Vec<Certificate>,
    expiry: SystemTime,
}

#[derive(Clone)]
struct Key(Arc<EcdsaKeyPair>);

struct CertResolver(sign::CertifiedKey);

// These must be kept in sync:
static SIGNATURE_ALG_RING_SIGNING: &ring::signature::EcdsaSigningAlgorithm =
    &ring::signature::ECDSA_P256_SHA256_ASN1_SIGNING;
const SIGNATURE_ALG_RUSTLS_SCHEME: SignatureScheme = SignatureScheme::ECDSA_NISTP256_SHA256;
const SIGNATURE_ALG_RUSTLS_ALGORITHM: internal::msgs::enums::SignatureAlgorithm =
    internal::msgs::enums::SignatureAlgorithm::ECDSA;
const TLS_VERSIONS: &[ProtocolVersion] = &[ProtocolVersion::TLSv1_3];

impl Credentials {
    pub fn load(identity: id::Name, roots_pem: &str, key_pkcs8: &[u8], csr: &[u8]) -> Result<Self> {
        let roots = {
            let mut roots = RootCertStore::empty();
            let (added, skipped) = roots
                .add_pem_file(&mut std::io::Cursor::new(roots_pem))
                .map_err(InvalidTrustRoots)?;
            if skipped != 0 {
                warn!("Skipped {} invalid trust anchors", skipped);
            }
            if added == 0 {
                return Err("no trust roots loaded".into());
            }
            roots
        };

        let key =
            EcdsaKeyPair::from_pkcs8(SIGNATURE_ALG_RING_SIGNING, key_pkcs8).map_err(InvalidKey)?;

        Ok(Self {
            roots,
            key: Arc::new(key),
            csr: csr.into(),
            identity,
            certificate: None,
        })
    }
}

impl id::Credentials for Credentials {
    fn name(&self) -> &id::Name {
        &self.identity
    }

    fn get_csr(&self) -> Vec<u8> {
        self.csr.to_vec()
    }

    fn set_crt(
        &mut self,
        leaf: Vec<u8>,
        intermediates: Vec<Vec<u8>>,
        expiry: SystemTime,
    ) -> Result<()> {
        let mut chain = Vec::with_capacity(intermediates.len() + 1);
        chain.push(Certificate(leaf));
        chain.extend(intermediates.into_iter().map(Certificate));

        self.certificate = Some(Crt { chain, expiry });

        Ok(())
    }
}

// === impl TrustAnchors ===

/*
let mut client = ClientConfig::new();

// XXX: Rustls's built-in verifiers don't let us tweak things as fully
// as we'd like (e.g. controlling the set of trusted signature
// algorithms), but they provide good enough defaults for now.
// TODO: lock down the verification further.
// TODO: Change Rustls's API to Avoid needing to clone `root_cert_store`.
client.root_store = roots;

// Disable session resumption for the time-being until resumption is
// more tested.
client.enable_tickets = false;

// Ensure the certificate is valid for the services we terminate for
// TLS. This assumes that server cert validation does the same or
// more validation than client cert validation.
//
// XXX: Rustls currently only provides access to a
// `ServerCertVerifier` through
// `ClientConfig::get_verifier()`.
//
// XXX: Once `ServerCertVerified` is exposed in Rustls's
// safe API, use it to pass proof to CertCertResolver::new....
//
// TODO: Restrict accepted signature algorithms.
static NO_OCSP: &[u8] = &[];
let crt_id = webpki::DNSNameRef::try_from_ascii(crt.id.as_bytes())
    .map_err(|e| InvalidCrt(TLSError::General(e.to_string())))?;
client
    .get_verifier()
    .verify_server_cert(&client.root_store, &crt.chain, crt_id, NO_OCSP)
    .map_err(InvalidCrt)?;
debug!("certified {}", crt.id);

// Enable client authentication.
client.client_auth_cert_resolver = Arc::new(CertResolver(sign::CertifiedKey::new(crt.chain, Arc::new(Box::new(key)))));
*/

/*
// Ask TLS clients for a certificate and accept any certificate issued
// by our trusted CA(s).
//
// XXX: Rustls's built-in verifiers don't let us tweak things as fully
// as we'd like (e.g. controlling the set of trusted signature
// algorithms), but they provide good enough defaults for now.
// TODO: lock down the verification further.
//
// TODO: Change Rustls's API to Avoid needing to clone `root_cert_store`.
let mut server = ServerConfig::new(AllowAnyAnonymousOrAuthenticatedClient::new(
    self.0.root_store.clone(),
));
server.versions = TLS_VERSIONS.to_vec();
server.cert_resolver = resolver;
*/

// === impl CertResolver ===

impl ResolvesClientCert for CertResolver {
    fn resolve(
        &self,
        _acceptable_issuers: &[&[u8]],
        sigschemes: &[SignatureScheme],
    ) -> Option<sign::CertifiedKey> {
        // The proxy's server-side doesn't send the list of acceptable issuers so don't bother
        // looking at `_acceptable_issuers`.
        if !sigschemes.contains(&SIGNATURE_ALG_RUSTLS_SCHEME) {
            debug!("Signature scheme not supported -> no certificate");
            return None;
        }

        Some(self.0.clone())
    }

    fn has_certs(&self) -> bool {
        true
    }
}

impl ResolvesServerCert for CertResolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<sign::CertifiedKey> {
        let server_name = hello.server_name().or_else(|| {
            debug!("no SNI -> no certificate");
            None
        })?;

        // Verify that our certificate is valid for the given SNI name.
        let c = self.0.cert.first()?;
        if let Err(error) = webpki::EndEntityCert::from(c.as_ref())
            .and_then(|c| c.verify_is_valid_for_dns_name(server_name))
        {
            debug!(%error, "Local certificate is not valid for SNI");
            return None;
        };

        if !hello.sigschemes().contains(&SIGNATURE_ALG_RUSTLS_SCHEME) {
            debug!("Signature scheme not supported -> no certificate");
            return None;
        }

        Some(self.0.clone())
    }
}

// === impl Key ===

impl sign::SigningKey for Key {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn sign::Signer>> {
        if !offered.contains(&SIGNATURE_ALG_RUSTLS_SCHEME) {
            return None;
        }

        Some(Box::new(self.clone()))
    }

    fn algorithm(&self) -> internal::msgs::enums::SignatureAlgorithm {
        SIGNATURE_ALG_RUSTLS_ALGORITHM
    }
}

impl sign::Signer for Key {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, TLSError> {
        let rng = rand::SystemRandom::new();
        self.0
            .sign(&rng, message)
            .map(|signature| signature.as_ref().to_owned())
            .map_err(|ring::error::Unspecified| TLSError::General("Signing Failed".to_owned()))
    }

    fn get_scheme(&self) -> SignatureScheme {
        SIGNATURE_ALG_RUSTLS_SCHEME
    }
}
