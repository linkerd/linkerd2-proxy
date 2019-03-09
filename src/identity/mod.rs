extern crate ring;
extern crate rustls;
extern crate untrusted;

use self::ring::rand;
use self::ring::signature::EcdsaKeyPair;
use self::rustls::RootCertStore;
use std::{fmt, fs, io, path::PathBuf, sync::Arc};

pub use self::ring::error::KeyRejected;

use convert::TryFrom;
use dns;

#[derive(Debug)]
pub struct Config {
    pub trust_anchors: TrustAnchors,
    pub key: Key,
    pub csr: CSR,
    pub identity: Identity,
    pub token: TokenSource,
}

#[derive(Clone)]
pub struct State {
    pub crt: Option<Crt>,
}

/// An endpoint's identity.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct Identity(Arc<dns::Name>);

#[derive(Clone)]
pub struct Crt(Arc<rustls::sign::CertifiedKey>);

/// A DER-encoded X.509 certificate signing request.
#[derive(Clone, Debug)]
pub struct CSR(Arc<Vec<u8>>);

#[derive(Clone, Debug)]
pub struct Key(Arc<EcdsaKeyPair>);

#[derive(Clone, Debug)]
pub struct TrustAnchors(Arc<RootCertStore>);

#[derive(Clone, Debug)]
pub struct TokenSource(Arc<PathBuf>);

// These must be kept in sync:
static SIGNATURE_ALG_RING_SIGNING: &ring::signature::EcdsaSigningAlgorithm =
    &ring::signature::ECDSA_P256_SHA256_ASN1_SIGNING;
const SIGNATURE_ALG_RUSTLS_SCHEME: rustls::SignatureScheme =
    rustls::SignatureScheme::ECDSA_NISTP256_SHA256;
const SIGNATURE_ALG_RUSTLS_ALGORITHM: rustls::internal::msgs::enums::SignatureAlgorithm =
    rustls::internal::msgs::enums::SignatureAlgorithm::ECDSA;

// === impl CSR ===

impl CSR {
    pub fn from_der(der: Vec<u8>) -> Option<Self> {
        if der.is_empty() {
            return None;
        }

        Some(CSR(Arc::new(der)))
    }
}

// === impl Key ===

impl Key {
    pub fn from_pkcs8(b: &[u8]) -> Result<Self, KeyRejected> {
        let i = untrusted::Input::from(b);
        let k = EcdsaKeyPair::from_pkcs8(SIGNATURE_ALG_RING_SIGNING, i)?;
        Ok(Key(Arc::new(k)))
    }
}

impl rustls::sign::SigningKey for Key {
    fn choose_scheme(
        &self,
        offered: &[rustls::SignatureScheme],
    ) -> Option<Box<rustls::sign::Signer>> {
        if offered.contains(&SIGNATURE_ALG_RUSTLS_SCHEME) {
            Some(Box::new(self.clone()))
        } else {
            None
        }
    }

    fn algorithm(&self) -> rustls::internal::msgs::enums::SignatureAlgorithm {
        SIGNATURE_ALG_RUSTLS_ALGORITHM
    }
}

impl rustls::sign::Signer for Key {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rustls::TLSError> {
        let rng = rand::SystemRandom::new();
        self.0
            .sign(&rng, untrusted::Input::from(message))
            .map(|signature| signature.as_ref().to_owned())
            .map_err(|ring::error::Unspecified| {
                rustls::TLSError::General("Signing Failed".to_owned())
            })
    }

    fn get_scheme(&self) -> rustls::SignatureScheme {
        SIGNATURE_ALG_RUSTLS_SCHEME
    }
}

// === impl Identity ===

impl From<dns::Name> for Identity {
    fn from(n: dns::Name) -> Self {
        Identity(Arc::new(n))
    }
}

impl Identity {
    pub fn from_sni_hostname(hostname: &[u8]) -> Result<Self, dns::InvalidName> {
        if hostname.last() == Some(&b'.') {
            return Err(dns::InvalidName); // SNI hostnames are implicitly absolute.
        }

        dns::Name::try_from(hostname).map(|n| n.into())
    }

    pub fn as_dns_name_ref(&self) -> webpki::DNSNameRef {
        self.0.as_dns_name_ref()
    }
}

impl AsRef<str> for Identity {
    fn as_ref(&self) -> &str {
        (*self.0).as_ref()
    }
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        fmt::Debug::fmt(&self.0, f)
    }
}

// === impl TrustAnchors ===

impl TokenSource {
    pub fn if_nonempty_file(s: &str) -> io::Result<Self> {
        let p = PathBuf::from(s);
        fs::read(p.clone()).and_then(|b| {
            if b.is_empty() {
                Err(io::Error::new(io::ErrorKind::Other.into(), "file is empty"))
            } else {
                Ok(TokenSource(Arc::new(p)))
            }
        })
    }
}

// === impl TrustAnchors ===

impl TrustAnchors {
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

        Some(TrustAnchors(Arc::new(roots)))
    }
}
