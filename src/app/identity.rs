extern crate ring;
extern crate rustls;
extern crate untrusted;
extern crate webpki;

use self::ring::signature::EcdsaKeyPair;
use std::{fs, io, path::PathBuf, sync::Arc};
use self::webpki::{DNSName, DNSNameRef};

use super::config::{
    parse,
    ENV_IDENTITY_END_ENTITY_DIR,
    ENV_IDENTITY_TRUST_ANCHORS,
    ENV_LOCAL_IDENTITY,
    ParseError,
    Strings,
};

// These must be kept in sync:
static SIGNATURE_ALG_RING_SIGNING: &ring::signature::EcdsaSigningAlgorithm =
    &ring::signature::ECDSA_P256_SHA256_ASN1_SIGNING;
const SIGNATURE_ALG_RUSTLS_SCHEME: rustls::SignatureScheme =
    rustls::SignatureScheme::ECDSA_NISTP256_SHA256;
const SIGNATURE_ALG_RUSTLS_ALGORITHM: rustls::internal::msgs::enums::SignatureAlgorithm =
    rustls::internal::msgs::enums::SignatureAlgorithm::ECDSA;


#[derive(Clone, Debug)]
pub struct Config {
    trust_anchors: rustls::RootCertStore,
    csr: Csr,
    key: Arc<EcdsaKeyPair>,
    name: DNSName,
}

/// A DER-encoded X.509 certificate signing request.
#[derive(Clone, Debug)]
pub struct Csr(Vec<u8>);

#[derive(Debug)]
pub enum Error {
    EmptyCSR,
    InvalidEnv,
    InvalidKey(ring::error::KeyRejected),
    InvalidEndEntityDir,
    InvalidTrustAnchors,
    Io(io::Error),
}

impl Config {
    fn parse(strings: &Strings) -> Result<Option<Self>, Box<dyn Error>> {
        match (
            parse(strings, ENV_IDENTITY_TRUST_ANCHORS, parse_cert_pool)
                .map_err(|_| Error:InvalidTrustAnchors)?,
            parse(strings, ENV_IDENTITY_END_ENTITY_DIR, parse_path)?,
                .map_err(|_| Error:InvalidEndEntityDir)?,
            parse(strings, ENV_LOCAL_IDENTITY, parse_identity)?.as_ref(),
        ) {
            (None, None, None) => Ok(None),
            (Some(trust_anchors), Some(end_entity_dir), Some(local_id)) => {
                let (key, csr) = load_dir(end_entity_dir)?;
                Ok(Some(Config {
                    csr,
                    key: Arc::new(key),
                    trust_anchors,
                }))
            }
            (trust_anchors, end_entity_dir, local_id) => {
                for (unset, name) in &[
                    (trust_anchors.is_none(), ENV_IDENTITY_TRUST_ANCHORS),
                    (end_entity_dir.is_none(), ENV_IDENTITY_END_ENTITY_DIR),
                    (local_id.is_none(), ENV_LOCAL_IDENTITY),
                ] {
                    if *unset {
                        error!("{} must be set when other identity variables are set.", name);
                    }
                }
                Err(Error::InvalidEnv)
            }
        }
    }
}

fn load_dir(dir: PathBuf) -> Result<(EcdsaKeyPair, Csr), Error> {
    let key = {
        let mut p = dir.clone();
        p.push("key.p8");

        let b = fs::read(p).map_err(Error::Io)?;
        EcdsaKeyPair::from_pkcs8(SIGNATURE_ALG_RING_SIGNING, untrusted::Input::from(&b))
            .map_err(Error::InvalidKey)?
    };

    let csr = {
        let mut p = dir;
        p.push("csr.der");

        let b = fs::read(p).map_err(Error::Io)?;
        if b.is_empty() {
            return Err(Error::EmptyCSR);
        }
        Csr(b)
    };

    Ok((key, csr))
}

fn parse_path(s: &str) -> Result<PathBuf, ParseError> {
    Ok(PathBuf::from(s))
}

pub(super) fn parse_identity(s: &str) -> Result<DNSName, ParseError> {
    DNSNameRef::try_from_ascii(s.as_bytes())
        .map(|r| r.to_owned())
        .map_err(|_| ParseError::NameError)
}


fn parse_cert_pool(s: &str) -> Result<rustls::RootCertStore, ParseError> {
    use std::io::Cursor;

    let mut root_cert_store = rustls::RootCertStore::empty();
    let (added, skipped) = root_cert_store
        .add_pem_file(&mut Cursor::new(s))
        .map_err(|_| ParseError::InvalidTrustAnchors)?;
    if skipped != 0 {
        warn!("skipped {} trust anchors in trust anchors file", skipped);
    }

    if added > 0 {
        Ok(root_cert_store)
    } else {
        Err(ParseError::InvalidTrustAnchors)
    }
}
