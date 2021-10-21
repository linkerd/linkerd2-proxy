#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]

mod client;
mod server;
#[cfg(feature = "test-util")]
pub mod test_util;

pub use self::{
    client::{ClientIo, Connect, ConnectFuture},
    server::{terminate, ServerIo, TerminateFuture},
};
use linkerd_identity as id;
pub use ring::error::KeyRejected;
use ring::{rand, signature::EcdsaKeyPair};
use std::{sync::Arc, time::SystemTime};
use thiserror::Error;
pub use tokio_rustls::rustls::*;
use tracing::{debug, warn};

#[derive(Clone, Debug)]
pub struct Key(Arc<EcdsaKeyPair>);

struct SigningKey(Arc<EcdsaKeyPair>);
struct Signer(Arc<EcdsaKeyPair>);

#[derive(Clone)]
pub struct TrustAnchors(Arc<ClientConfig>);

#[derive(Clone, Debug)]
pub struct Crt {
    id: id::LocalId,
    expiry: SystemTime,
    chain: Vec<Certificate>,
}

#[derive(Clone)]
pub struct CrtKey {
    id: id::LocalId,
    expiry: SystemTime,
    client_config: Arc<ClientConfig>,
    server_config: Arc<ServerConfig>,
}

struct CertResolver(sign::CertifiedKey);

#[derive(Clone, Debug, Error)]
#[error(transparent)]
pub struct InvalidCrt(TLSError);

// These must be kept in sync:
static SIGNATURE_ALG_RING_SIGNING: &ring::signature::EcdsaSigningAlgorithm =
    &ring::signature::ECDSA_P256_SHA256_ASN1_SIGNING;
const SIGNATURE_ALG_RUSTLS_SCHEME: SignatureScheme = SignatureScheme::ECDSA_NISTP256_SHA256;
const SIGNATURE_ALG_RUSTLS_ALGORITHM: internal::msgs::enums::SignatureAlgorithm =
    internal::msgs::enums::SignatureAlgorithm::ECDSA;
const TLS_VERSIONS: &[ProtocolVersion] = &[ProtocolVersion::TLSv1_3];

// === impl Key ===

impl Key {
    pub fn from_pkcs8(b: &[u8]) -> Result<Self, KeyRejected> {
        let k = EcdsaKeyPair::from_pkcs8(SIGNATURE_ALG_RING_SIGNING, b)?;
        Ok(Key(Arc::new(k)))
    }
}

impl sign::SigningKey for SigningKey {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn sign::Signer>> {
        if offered.contains(&SIGNATURE_ALG_RUSTLS_SCHEME) {
            Some(Box::new(Signer(self.0.clone())))
        } else {
            None
        }
    }

    fn algorithm(&self) -> internal::msgs::enums::SignatureAlgorithm {
        SIGNATURE_ALG_RUSTLS_ALGORITHM
    }
}

impl sign::Signer for Signer {
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

// === impl TrustAnchors ===

impl TrustAnchors {
    #[cfg(feature = "test-util")]
    fn empty() -> Self {
        TrustAnchors(Arc::new(ClientConfig::new()))
    }

    pub fn from_pem(s: &str) -> Option<Self> {
        use std::io::Cursor;

        let mut roots = RootCertStore::empty();
        let (added, skipped) = roots.add_pem_file(&mut Cursor::new(s)).ok()?;
        if skipped != 0 {
            warn!("skipped {} trust anchors in trust anchors file", skipped);
        }
        if added == 0 {
            return None;
        }

        let mut c = ClientConfig::new();

        // XXX: Rustls's built-in verifiers don't let us tweak things as fully
        // as we'd like (e.g. controlling the set of trusted signature
        // algorithms), but they provide good enough defaults for now.
        // TODO: lock down the verification further.
        // TODO: Change Rustls's API to Avoid needing to clone `root_cert_store`.
        c.root_store = roots;

        // Disable session resumption for the time-being until resumption is
        // more tested.
        c.enable_tickets = false;

        Some(TrustAnchors(Arc::new(c)))
    }

    pub fn certify(&self, key: Key, crt: Crt) -> Result<CrtKey, InvalidCrt> {
        let mut client = self.0.as_ref().clone();

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
        let crt_id = webpki::DNSNameRef::try_from_ascii((***crt.id).as_bytes())
            .map_err(|e| InvalidCrt(TLSError::General(e.to_string())))?;
        client
            .get_verifier()
            .verify_server_cert(&client.root_store, &crt.chain, crt_id, NO_OCSP)
            .map_err(InvalidCrt)?;
        debug!("certified {}", crt.id);

        let k = SigningKey(key.0);
        let key = sign::CertifiedKey::new(crt.chain, Arc::new(Box::new(k)));
        let resolver = Arc::new(CertResolver(key));

        // Enable client authentication.
        client.client_auth_cert_resolver = resolver.clone();

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

        Ok(CrtKey {
            id: crt.id,
            expiry: crt.expiry,
            client_config: Arc::new(client),
            server_config: Arc::new(server),
        })
    }

    pub fn client_config(&self) -> Arc<ClientConfig> {
        self.0.clone()
    }
}

impl std::fmt::Debug for TrustAnchors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrustAnchors").finish()
    }
}

// === Crt ===

impl Crt {
    pub fn new(
        id: id::LocalId,
        leaf: Vec<u8>,
        intermediates: Vec<Vec<u8>>,
        expiry: SystemTime,
    ) -> Self {
        let mut chain = Vec::with_capacity(intermediates.len() + 1);
        chain.push(Certificate(leaf));
        chain.extend(intermediates.into_iter().map(Certificate));

        Self { id, chain, expiry }
    }

    pub fn name(&self) -> &id::Name {
        &self.id.0
    }
}

impl From<&'_ Crt> for id::LocalId {
    fn from(crt: &Crt) -> id::LocalId {
        crt.id.clone()
    }
}

// === CrtKey ===

impl CrtKey {
    pub fn name(&self) -> &id::Name {
        &self.id.0
    }

    pub fn expiry(&self) -> SystemTime {
        self.expiry
    }

    pub fn id(&self) -> &id::LocalId {
        &self.id
    }

    pub fn client_config(&self) -> Arc<ClientConfig> {
        self.client_config.clone()
    }

    pub fn server_config(&self) -> Arc<ServerConfig> {
        self.server_config.clone()
    }
}

impl std::fmt::Debug for CrtKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        f.debug_struct("CrtKey")
            .field("id", &self.id)
            .field("expiry", &self.expiry)
            .finish()
    }
}

// === impl CertResolver ===

impl ResolvesClientCert for CertResolver {
    fn resolve(
        &self,
        _acceptable_issuers: &[&[u8]],
        sigschemes: &[SignatureScheme],
    ) -> Option<sign::CertifiedKey> {
        // The proxy's server-side doesn't send the list of acceptable issuers so
        // don't bother looking at `_acceptable_issuers`.
        self.resolve_(sigschemes)
    }

    fn has_certs(&self) -> bool {
        true
    }
}

impl CertResolver {
    fn resolve_(&self, sigschemes: &[SignatureScheme]) -> Option<sign::CertifiedKey> {
        if !sigschemes.contains(&SIGNATURE_ALG_RUSTLS_SCHEME) {
            debug!("signature scheme not supported -> no certificate");
            return None;
        }
        Some(self.0.clone())
    }
}

impl ResolvesServerCert for CertResolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<sign::CertifiedKey> {
        let server_name = if let Some(server_name) = hello.server_name() {
            server_name
        } else {
            debug!("no SNI -> no certificate");
            return None;
        };

        // Verify that our certificate is valid for the given SNI name.
        let c = (&self.0.cert)
            .first()
            .map(Certificate::as_ref)
            .unwrap_or(&[]); // An empty input will fail to parse.
        if let Err(err) =
            webpki::EndEntityCert::from(c).and_then(|c| c.verify_is_valid_for_dns_name(server_name))
        {
            debug!(
                "our certificate is not valid for the SNI name -> no certificate: {:?}",
                err
            );
            return None;
        }

        self.resolve_(hello.sigschemes())
    }
}
