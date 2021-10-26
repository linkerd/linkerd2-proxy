use super::params::*;
use linkerd_error::Result;
use linkerd_proxy_identity as id;
use ring::{rand, signature::EcdsaKeyPair};
use std::sync::Arc;
use tokio::sync::watch;
use tokio_rustls::rustls;
use tracing::debug;

pub struct Store {
    roots: rustls::RootCertStore,
    key: Arc<EcdsaKeyPair>,
    csr: Arc<[u8]>,
    identity: id::Name,
    client_tx: watch::Sender<Arc<rustls::ClientConfig>>,
    server_tx: watch::Sender<Arc<rustls::ServerConfig>>,
}

#[derive(Clone)]
struct Key(Arc<EcdsaKeyPair>);

struct CertResolver(rustls::sign::CertifiedKey);

// === impl Store ===

impl Store {
    pub(super) fn new(
        roots: rustls::RootCertStore,
        key: EcdsaKeyPair,
        csr: &[u8],
        identity: id::Name,
        client_tx: watch::Sender<Arc<rustls::ClientConfig>>,
        server_tx: watch::Sender<Arc<rustls::ServerConfig>>,
    ) -> Self {
        Self {
            roots,
            key: Arc::new(key),
            csr: csr.into(),
            identity,
            client_tx,
            server_tx,
        }
    }
}

impl id::Credentials for Store {
    fn get_dns_name(&self) -> &id::Name {
        &self.identity
    }

    fn get_certificate_signing_request(&self) -> id::DerX509 {
        id::DerX509(self.csr.to_vec())
    }

    fn set_certificate(
        &mut self,
        id::DerX509(leaf): id::DerX509,
        intermediates: Vec<id::DerX509>,
        _expiry: std::time::SystemTime,
    ) -> Result<()> {
        let mut chain = Vec::with_capacity(intermediates.len() + 1);
        chain.push(rustls::Certificate(leaf));
        chain.extend(
            intermediates
                .into_iter()
                .map(|id::DerX509(der)| rustls::Certificate(der)),
        );

        let mut client = rustls::ClientConfig::new();
        client.ciphersuites = TLS_SUPPORTED_CIPHERSUITES.to_vec();

        // XXX: Rustls's built-in verifiers don't let us tweak things as fully
        // as we'd like (e.g. controlling the set of trusted signature
        // algorithms), but they provide good enough defaults for now.
        // TODO: lock down the verification further.
        // TODO: Change Rustls's API to avoid needing to clone `root_cert_store`.
        client.root_store = self.roots.clone();

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
        let crt_id = webpki::DNSNameRef::try_from_ascii(self.identity.as_bytes())
            .expect("identity must be a valid DNS name");
        static NO_OCSP: &[u8] = &[];
        client
            .get_verifier()
            .verify_server_cert(&self.roots, &*chain, crt_id, NO_OCSP)?;
        debug!("Certified");

        let resolver = Arc::new(CertResolver(rustls::sign::CertifiedKey::new(
            chain,
            Arc::new(Box::new(Key(self.key.clone()))),
        )));

        // Enable client authentication.
        client.client_auth_cert_resolver = resolver.clone();

        let _ = self.client_tx.send(client.into());

        // Ask TLS clients for a certificate and accept any certificate issued
        // by our trusted CA(s).
        //
        // XXX: Rustls's built-in verifiers don't let us tweak things as fully
        // as we'd like (e.g. controlling the set of trusted signature
        // algorithms), but they provide good enough defaults for now.
        // TODO: lock down the verification further.
        //
        // TODO: Change Rustls's API to avoid needing to clone `root_cert_store`.
        let mut server = rustls::ServerConfig::new(
            rustls::AllowAnyAnonymousOrAuthenticatedClient::new(self.roots.clone()),
        );
        server.versions = TLS_VERSIONS.to_vec();
        server.cert_resolver = resolver;
        server.ciphersuites = TLS_SUPPORTED_CIPHERSUITES.to_vec();

        let _ = self.server_tx.send(server.into());

        Ok(())
    }
}

// === impl Key ===

impl rustls::sign::SigningKey for Key {
    fn choose_scheme(
        &self,
        offered: &[rustls::SignatureScheme],
    ) -> Option<Box<dyn rustls::sign::Signer>> {
        if !offered.contains(&SIGNATURE_ALG_RUSTLS_SCHEME) {
            return None;
        }

        Some(Box::new(self.clone()))
    }

    fn algorithm(&self) -> rustls::internal::msgs::enums::SignatureAlgorithm {
        SIGNATURE_ALG_RUSTLS_ALGORITHM
    }
}

impl rustls::sign::Signer for Key {
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

// === impl CertResolver ===

impl CertResolver {
    #[inline]
    fn resolve_(
        &self,
        sigschemes: &[rustls::SignatureScheme],
    ) -> Option<rustls::sign::CertifiedKey> {
        if !sigschemes.contains(&SIGNATURE_ALG_RUSTLS_SCHEME) {
            debug!("Signature scheme not supported -> no certificate");
            return None;
        }

        Some(self.0.clone())
    }
}

impl rustls::ResolvesClientCert for CertResolver {
    fn resolve(
        &self,
        _acceptable_issuers: &[&[u8]],
        sigschemes: &[rustls::SignatureScheme],
    ) -> Option<rustls::sign::CertifiedKey> {
        self.resolve_(sigschemes)
    }

    fn has_certs(&self) -> bool {
        true
    }
}

impl rustls::ResolvesServerCert for CertResolver {
    fn resolve(&self, hello: rustls::ClientHello<'_>) -> Option<rustls::sign::CertifiedKey> {
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

        self.resolve_(hello.sigschemes())
    }
}
