use std::{fmt, sync::Arc};

use super::{config, rustls, untrusted, webpki};
use ring::{self, rand, signature};

/// Manages the use of the private key and certificate.
///
/// Authentication is symmetric with respect to the client/server roles, so the
/// same certificate and private key is used for both roles.
pub struct CertResolver {
    certified_key: Option<rustls::sign::CertifiedKey>,
}

impl fmt::Debug for CertResolver {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        f.debug_struct("CertResolver").finish()
    }
}

struct SigningKey {
    signer: Signer,
}

#[derive(Clone)]
struct Signer {
    private_key: Arc<signature::EcdsaKeyPair>,
}

impl CertResolver {
    /// Returns a new `CertResolver` that has a certificate (chain) verified to
    /// have been issued by one of the given trust anchors.
    ///
    /// TODO: Have the caller pass in a `rustls::ServerCertVerified` as evidence
    /// that the certificate chain was validated, once Rustls's (safe) API
    /// supports that.
    ///
    /// TODO: Verify that the public key of the certificate matches the private
    /// key.
    pub fn new(
        _certificate_was_validated: (), // TODO: `rustls::ServerCertVerified`.
        cert_chain: Vec<rustls::Certificate>,
        private_key: untrusted::Input,
    ) -> Result<Self, config::Error> {
        let private_key =
            signature::EcdsaKeyPair::from_pkcs8(config::SIGNATURE_ALG_RING_SIGNING, private_key)
                .map_err(config::Error::InvalidPrivateKey)?;

        let signer = Signer {
            private_key: Arc::new(private_key),
        };
        let signing_key = SigningKey { signer };
        let certified_key = Some(rustls::sign::CertifiedKey::new(
            cert_chain,
            Arc::new(Box::new(signing_key)),
        ));
        Ok(Self { certified_key })
    }

    /// Returns a new `CertResolver` which indicates that we don't yet have
    /// a certificate.
    ///
    /// This is used in order to fail handshakes when the TLS configuration
    /// hasn't been loaded yet. The returned `CertResolver` implements
    /// `rustls::ResolvesClientCert` and `rustls::ResolvesServerCert`, but
    /// will always returns `None`.
    pub fn empty() -> Self {
        Self {
            certified_key: None,
        }
    }

    fn resolve_(
        &self,
        sigschemes: &[rustls::SignatureScheme],
    ) -> Option<rustls::sign::CertifiedKey> {
        if !sigschemes.contains(&config::SIGNATURE_ALG_RUSTLS_SCHEME) {
            debug!("signature scheme not supported -> no certificate");
            return None;
        }

        self.certified_key.as_ref().cloned()
    }
}

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
        self.certified_key.is_some()
    }
}

impl rustls::ResolvesServerCert for CertResolver {
    fn resolve(
        &self,
        server_name: Option<webpki::DNSNameRef>,
        sigschemes: &[rustls::SignatureScheme],
    ) -> Option<rustls::sign::CertifiedKey> {
        let server_name = if let Some(server_name) = server_name {
            server_name
        } else {
            debug!("no SNI -> no certificate");
            return None;
        };

        // Verify that our certificate is valid for the given SNI name.
        if let Err(err) = super::parse_end_entity_cert(&self.certified_key.as_ref()?.cert)
            .and_then(|cert| cert.verify_is_valid_for_dns_name(server_name))
        {
            debug!(
                "our certificate is not valid for the SNI name -> no certificate: {:?}",
                err
            );
            return None;
        }

        self.resolve_(sigschemes)
    }
}

impl rustls::sign::SigningKey for SigningKey {
    fn choose_scheme(
        &self,
        offered: &[rustls::SignatureScheme],
    ) -> Option<Box<rustls::sign::Signer>> {
        if offered.contains(&config::SIGNATURE_ALG_RUSTLS_SCHEME) {
            Some(Box::new(self.signer.clone()))
        } else {
            None
        }
    }

    fn algorithm(&self) -> rustls::internal::msgs::enums::SignatureAlgorithm {
        config::SIGNATURE_ALG_RUSTLS_ALGORITHM
    }
}

impl rustls::sign::Signer for Signer {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rustls::TLSError> {
        let rng = rand::SystemRandom::new();
        self.private_key
            .sign(&rng, untrusted::Input::from(message))
            .map(|signature| signature.as_ref().to_owned())
            .map_err(|ring::error::Unspecified| {
                rustls::TLSError::General("Signing Failed".to_owned())
            })
    }

    fn get_scheme(&self) -> rustls::SignatureScheme {
        config::SIGNATURE_ALG_RUSTLS_SCHEME
    }
}
