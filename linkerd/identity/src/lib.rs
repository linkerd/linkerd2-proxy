#![deny(warnings, rust_2018_idioms)]

pub use ring::error::KeyRejected;
use ring::rand;
use ring::signature::EcdsaKeyPair;
use std::{convert::TryFrom, error::Error, fmt, fs, io, str::FromStr, sync::Arc, time::SystemTime};
use tracing::{debug, warn};

#[cfg(any(test, feature = "test-util"))]
pub mod test_util;

pub use linkerd_dns_name::InvalidName;

/// A DER-encoded X.509 certificate signing request.
#[derive(Clone, Debug)]
pub struct Csr(Arc<Vec<u8>>);

/// An endpoint's identity.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct Name(Arc<linkerd_dns_name::Name>);

#[derive(Clone, Debug)]
pub struct Key(Arc<EcdsaKeyPair>);

struct SigningKey(Arc<EcdsaKeyPair>);
struct Signer(Arc<EcdsaKeyPair>);

#[derive(Clone)]
pub struct TrustAnchors(Arc<rustls::ClientConfig>);

#[derive(Clone, Debug)]
pub struct TokenSource(Arc<String>);

#[derive(Clone, Debug)]
pub struct Crt {
    id: LocalId,
    expiry: SystemTime,
    chain: Vec<rustls::Certificate>,
}

#[derive(Clone)]
pub struct CrtKey {
    id: LocalId,
    expiry: SystemTime,
    client_config: Arc<rustls::ClientConfig>,
    server_config: Arc<rustls::ServerConfig>,
}

struct CertResolver(rustls::sign::CertifiedKey);

#[derive(Clone, Debug)]
pub struct InvalidCrt(rustls::TLSError);

/// A newtype for local server identities.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct LocalId(pub Name);

// These must be kept in sync:
static SIGNATURE_ALG_RING_SIGNING: &ring::signature::EcdsaSigningAlgorithm =
    &ring::signature::ECDSA_P256_SHA256_ASN1_SIGNING;
const SIGNATURE_ALG_RUSTLS_SCHEME: rustls::SignatureScheme =
    rustls::SignatureScheme::ECDSA_NISTP256_SHA256;
const SIGNATURE_ALG_RUSTLS_ALGORITHM: rustls::internal::msgs::enums::SignatureAlgorithm =
    rustls::internal::msgs::enums::SignatureAlgorithm::ECDSA;
const TLS_VERSIONS: &[rustls::ProtocolVersion] = &[rustls::ProtocolVersion::TLSv1_2];

// === impl Csr ===

impl Csr {
    pub fn from_der(der: Vec<u8>) -> Option<Self> {
        if der.is_empty() {
            return None;
        }

        Some(Csr(Arc::new(der)))
    }

    pub fn to_vec(&self) -> Vec<u8> {
        self.0.to_vec()
    }
}

// === impl Key ===

impl Key {
    pub fn from_pkcs8(b: &[u8]) -> Result<Self, KeyRejected> {
        let k = EcdsaKeyPair::from_pkcs8(SIGNATURE_ALG_RING_SIGNING, b)?;
        Ok(Key(Arc::new(k)))
    }
}

impl rustls::sign::SigningKey for SigningKey {
    fn choose_scheme(
        &self,
        offered: &[rustls::SignatureScheme],
    ) -> Option<Box<dyn rustls::sign::Signer>> {
        if offered.contains(&SIGNATURE_ALG_RUSTLS_SCHEME) {
            Some(Box::new(Signer(self.0.clone())))
        } else {
            None
        }
    }

    fn algorithm(&self) -> rustls::internal::msgs::enums::SignatureAlgorithm {
        SIGNATURE_ALG_RUSTLS_ALGORITHM
    }
}

impl rustls::sign::Signer for Signer {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rustls::TLSError> {
        let rng = rand::SystemRandom::new();
        self.0
            .sign(&rng, message)
            .map(|signature| signature.as_ref().to_owned())
            .map_err(|ring::error::Unspecified| {
                rustls::TLSError::General("Signing Failed".to_owned())
            })
    }

    fn get_scheme(&self) -> rustls::SignatureScheme {
        SIGNATURE_ALG_RUSTLS_SCHEME
    }
}

// === impl Name ===

impl From<linkerd_dns_name::Name> for Name {
    fn from(n: linkerd_dns_name::Name) -> Self {
        Name(Arc::new(n))
    }
}

impl<'t> Into<webpki::DNSNameRef<'t>> for &'t LocalId {
    fn into(self) -> webpki::DNSNameRef<'t> {
        (&self.0).into()
    }
}

impl FromStr for Name {
    type Err = InvalidName;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.as_bytes().last() == Some(&b'.') {
            return Err(InvalidName); // SNI hostnames are implicitly absolute.
        }

        linkerd_dns_name::Name::from_str(s).map(|n| Name(Arc::new(n)))
    }
}

impl TryFrom<&[u8]> for Name {
    type Error = InvalidName;

    fn try_from(s: &[u8]) -> Result<Self, Self::Error> {
        if s.last() == Some(&b'.') {
            return Err(InvalidName); // SNI hostnames are implicitly absolute.
        }

        linkerd_dns_name::Name::try_from(s).map(|n| Name(Arc::new(n)))
    }
}

impl<'t> Into<webpki::DNSNameRef<'t>> for &'t Name {
    fn into(self) -> webpki::DNSNameRef<'t> {
        self.0.as_ref().into()
    }
}

impl AsRef<str> for Name {
    fn as_ref(&self) -> &str {
        (*self.0).as_ref()
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        fmt::Display::fmt(&self.0, f)
    }
}

// === impl TokenSource ===

impl TokenSource {
    pub fn if_nonempty_file(p: String) -> io::Result<Self> {
        let ts = TokenSource(Arc::new(p));
        ts.load().map(|_| ts)
    }

    pub fn load(&self) -> io::Result<Vec<u8>> {
        let t = fs::read(self.0.as_str())?;

        if t.is_empty() {
            return Err(io::Error::new(io::ErrorKind::Other, "token is empty"));
        }

        Ok(t)
    }
}

// === impl TrustAnchors ===

impl TrustAnchors {
    #[cfg(any(test, feature = "test-util"))]
    fn empty() -> Self {
        TrustAnchors(Arc::new(rustls::ClientConfig::new()))
    }

    pub fn from_pem(s: &str) -> Option<Self> {
        use std::io::Cursor;

        let mut roots = rustls::RootCertStore::empty();
        let (added, skipped) = roots.add_pem_file(&mut Cursor::new(s)).ok()?;
        if skipped != 0 {
            warn!("skipped {} trust anchors in trust anchors file", skipped);
        }
        if added == 0 {
            return None;
        }

        let mut c = rustls::ClientConfig::new();

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
        // `rustls::ClientConfig::get_verifier()`.
        //
        // XXX: Once `rustls::ServerCertVerified` is exposed in Rustls's
        // safe API, use it to pass proof to CertCertResolver::new....
        //
        // TODO: Restrict accepted signatutre algorithms.
        static NO_OCSP: &[u8] = &[];
        client
            .get_verifier()
            .verify_server_cert(&client.root_store, &crt.chain, (&crt.id).into(), NO_OCSP)
            .map_err(InvalidCrt)?;
        debug!("certified {}", crt.id);

        let k = SigningKey(key.0);
        let key = rustls::sign::CertifiedKey::new(crt.chain, Arc::new(Box::new(k)));
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
        let mut server = rustls::ServerConfig::new(
            rustls::AllowAnyAnonymousOrAuthenticatedClient::new(self.0.root_store.clone()),
        );
        server.versions = TLS_VERSIONS.to_vec();
        server.cert_resolver = resolver;

        Ok(CrtKey {
            id: crt.id,
            expiry: crt.expiry,
            client_config: Arc::new(client),
            server_config: Arc::new(server),
        })
    }
}

impl Into<Arc<rustls::ClientConfig>> for &'_ TrustAnchors {
    fn into(self) -> Arc<rustls::ClientConfig> {
        self.0.clone()
    }
}

impl fmt::Debug for TrustAnchors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrustAnchors").finish()
    }
}

// === Crt ===

impl Crt {
    pub fn new(
        id: LocalId,
        leaf: Vec<u8>,
        intermediates: Vec<Vec<u8>>,
        expiry: SystemTime,
    ) -> Self {
        let mut chain = Vec::with_capacity(intermediates.len() + 1);
        chain.push(rustls::Certificate(leaf));
        chain.extend(intermediates.into_iter().map(rustls::Certificate));

        Self { id, chain, expiry }
    }

    pub fn name(&self) -> &Name {
        self.id.as_ref()
    }
}

impl Into<LocalId> for &'_ Crt {
    fn into(self) -> LocalId {
        self.id.clone()
    }
}

// === CrtKey ===

impl CrtKey {
    pub fn name(&self) -> &Name {
        self.id.as_ref()
    }

    pub fn expiry(&self) -> SystemTime {
        self.expiry
    }
}

impl Into<LocalId> for &'_ CrtKey {
    fn into(self) -> LocalId {
        self.id.clone()
    }
}

impl Into<Arc<rustls::ClientConfig>> for &'_ CrtKey {
    fn into(self) -> Arc<rustls::ClientConfig> {
        self.client_config.clone()
    }
}

impl Into<Arc<rustls::ServerConfig>> for &'_ CrtKey {
    fn into(self) -> Arc<rustls::ServerConfig> {
        self.server_config.clone()
    }
}

impl fmt::Debug for CrtKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        f.debug_struct("CrtKey")
            .field("id", &self.id)
            .field("expiry", &self.expiry)
            .finish()
    }
}

// === impl CertResolver ===

impl rustls::ResolvesClientCert for CertResolver {
    fn resolve(
        &self,
        _acceptable_issuers: &[&[u8]],
        sigschemes: &[rustls::SignatureScheme],
    ) -> Option<rustls::sign::CertifiedKey> {
        // The proxy's server-side doesn't send the list of acceptable issuers so
        // don't bother looking at `_acceptable_issuers`.
        self.resolve_(sigschemes)
    }

    fn has_certs(&self) -> bool {
        true
    }
}

impl CertResolver {
    fn resolve_(
        &self,
        sigschemes: &[rustls::SignatureScheme],
    ) -> Option<rustls::sign::CertifiedKey> {
        if !sigschemes.contains(&SIGNATURE_ALG_RUSTLS_SCHEME) {
            debug!("signature scheme not supported -> no certificate");
            return None;
        }
        Some(self.0.clone())
    }
}

impl rustls::ResolvesServerCert for CertResolver {
    fn resolve(&self, hello: rustls::ClientHello<'_>) -> Option<rustls::sign::CertifiedKey> {
        let server_name = if let Some(server_name) = hello.server_name() {
            server_name
        } else {
            debug!("no SNI -> no certificate");
            return None;
        };

        // Verify that our certificate is valid for the given SNI name.
        let c = (&self.0.cert)
            .first()
            .map(rustls::Certificate::as_ref)
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

// === impl LocalId ===

impl From<Name> for LocalId {
    fn from(n: Name) -> Self {
        Self(n)
    }
}

impl Into<Name> for LocalId {
    fn into(self) -> Name {
        self.0
    }
}

impl AsRef<Name> for LocalId {
    fn as_ref(&self) -> &Name {
        &self.0
    }
}

impl fmt::Display for LocalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

// === impl InvalidCrt ===

impl fmt::Display for InvalidCrt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Error for InvalidCrt {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.0.source()
    }
}

#[cfg(test)]
mod tests {
    use super::test_util::*;

    #[test]
    fn can_construct_client_and_server_config_from_valid_settings() {
        FOO_NS1.validate().expect("foo.ns1 must be valid");
    }

    #[test]
    fn recognize_ca_did_not_issue_cert() {
        let s = Identity {
            trust_anchors: include_bytes!("testdata/ca2.pem"),
            ..FOO_NS1
        };
        assert!(s.validate().is_err(), "ca2 should not validate foo.ns1");
    }

    #[test]
    fn recognize_cert_is_not_valid_for_identity() {
        let s = Identity {
            crt: BAR_NS1.crt,
            key: BAR_NS1.key,
            ..FOO_NS1
        };
        assert!(s.validate().is_err(), "identity should not be valid");
    }

    #[test]
    #[ignore] // XXX this doesn't fail because we don't actually check the key against the cert...
    fn recognize_private_key_is_not_valid_for_cert() {
        let s = Identity {
            key: BAR_NS1.key,
            ..FOO_NS1
        };
        assert!(s.validate().is_err(), "identity should not be valid");
    }
}
