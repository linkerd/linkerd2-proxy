extern crate ring;
extern crate rustls;
extern crate untrusted;

use self::ring::signature::EcdsaKeyPair;
use std::{fs, io, path::PathBuf, sync::Arc};

use super::config::{
    parse, Error as EnvError, ParseError, Strings, ENV_IDENTITY_DISABLED,
    ENV_IDENTITY_END_ENTITY_DIR, ENV_IDENTITY_LOCAL_IDENTITY, ENV_IDENTITY_TRUST_ANCHORS,
};
use dns;
use Conditional;

// These must be kept in sync:
static SIGNATURE_ALG_RING_SIGNING: &ring::signature::EcdsaSigningAlgorithm =
    &ring::signature::ECDSA_P256_SHA256_ASN1_SIGNING;
// const SIGNATURE_ALG_RUSTLS_SCHEME: rustls::SignatureScheme =
//     rustls::SignatureScheme::ECDSA_NISTP256_SHA256;
// const SIGNATURE_ALG_RUSTLS_ALGORITHM: rustls::internal::msgs::enums::SignatureAlgorithm =
//     rustls::internal::msgs::enums::SignatureAlgorithm::ECDSA;

#[derive(Clone, Debug)]
pub struct Config {
    trust_anchors: rustls::RootCertStore,
    csr: Csr,
    key: Arc<EcdsaKeyPair>,
    name: dns::Name,
}

/// A DER-encoded X.509 certificate signing request.
#[derive(Clone, Debug)]
pub struct Csr(Vec<u8>);

#[derive(Debug)]
pub enum Error {
    EmptyCSR,
    InvalidKey(ring::error::KeyRejected),
    Io(io::Error),
}

#[derive(Clone, Copy, Debug)]
pub struct Disabled(());
pub type ConditionalConfig = Conditional<Config, Disabled>;

impl Config {
    pub fn parse<S: Strings>(strings: &S) -> Result<ConditionalConfig, EnvError> {
        let ta = parse(strings, ENV_IDENTITY_TRUST_ANCHORS, parse_cert_pool);
        let ee = parse(strings, ENV_IDENTITY_END_ENTITY_DIR, parse_path);
        let li = parse(strings, ENV_IDENTITY_LOCAL_IDENTITY, super::config::parse_dns_name);

        let disabled = strings
            .get(ENV_IDENTITY_DISABLED)?
            .map(|d| !d.is_empty())
            .unwrap_or(false);
        match (disabled, ta?, ee?, li?) {
            (false, Some(trust_anchors), Some(end_entity_dir), Some(name)) => {
                let (key, csr) = load_dir(end_entity_dir).map_err(|e| {
                    error!("Invalid end-entity directory: {:?}", e);
                    EnvError::InvalidEnvVar
                })?;
                Ok(Conditional::Some(Config {
                    name,
                    csr,
                    key: Arc::new(key),
                    trust_anchors,
                }))
            }
            (true, None, None, None) => Ok(Conditional::None(Disabled(()))),
            (false, None, None, None) => {
                error!(
                    "{} must be set or identity configuration must be specified.",
                    ENV_IDENTITY_DISABLED
                );
                Err(EnvError::InvalidEnvVar)
            }
            (disabled, trust_anchors, end_entity_dir, local_id) => {
                if disabled {
                    error!(
                        "{} must be unset when other identity variables are set.",
                        ENV_IDENTITY_DISABLED,
                    );
                }
                for (unset, name) in &[
                    (trust_anchors.is_none(), ENV_IDENTITY_TRUST_ANCHORS),
                    (end_entity_dir.is_none(), ENV_IDENTITY_END_ENTITY_DIR),
                    (local_id.is_none(), ENV_IDENTITY_LOCAL_IDENTITY),
                ] {
                    if *unset {
                        error!(
                            "{} must be set when other identity variables are set.",
                            name
                        );
                    }
                }
                Err(EnvError::InvalidEnvVar)
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
