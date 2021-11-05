use super::params::*;
use linkerd_error::Result;
use linkerd_identity as id;
use ring::{rand, signature::EcdsaKeyPair};
use std::{convert::TryFrom, sync::Arc};
use tokio::sync::watch;
use tokio_rustls::rustls;
use tracing::debug;

pub struct Store {
    roots: rustls::RootCertStore,
    server_cert_verifier: Arc<dyn rustls::client::ServerCertVerifier>,
    key: Arc<EcdsaKeyPair>,
    csr: Arc<[u8]>,
    name: id::Name,
    client_tx: watch::Sender<Arc<rustls::ClientConfig>>,
    server_tx: watch::Sender<Arc<rustls::ServerConfig>>,
}

#[derive(Clone)]
struct Key(Arc<EcdsaKeyPair>);

#[derive(Clone)]
pub(super) struct CertResolver(Arc<rustls::sign::CertifiedKey>);

// === impl Store ===

impl Store {
    pub(super) fn new(
        roots: rustls::RootCertStore,
        server_cert_verifier: Arc<dyn rustls::client::ServerCertVerifier>,
        key: EcdsaKeyPair,
        csr: &[u8],
        name: id::Name,
        client_tx: watch::Sender<Arc<rustls::ClientConfig>>,
        server_tx: watch::Sender<Arc<rustls::ServerConfig>>,
    ) -> Self {
        Self {
            roots,
            key: Arc::new(key),
            server_cert_verifier,
            csr: csr.into(),
            name,
            client_tx,
            server_tx,
        }
    }

    /// Builds a new TLS client configuration.
    fn client(&self, resolver: CertResolver) -> rustls::ClientConfig {}

    /// Builds a new TLS server configuration.
    fn server(&self, resolver: CertResolver) -> rustls::ServerConfig {
        // Ask TLS clients for a certificate and accept any certificate issued by our trusted CA(s).
        //
        // XXX: Rustls's built-in verifiers don't let us tweak things as fully as we'd like (e.g.
        // controlling the set of trusted signature algorithms), but they provide good enough
        // defaults for now.
        // TODO: lock down the verification further.
        let client_cert_verifier =
            rustls::server::AllowAnyAnonymousOrAuthenticatedClient::new(self.roots.clone());
        rustls::ServerConfig::builder()
            .with_cipher_suites(TLS_SUPPORTED_CIPHERSUITES)
            .with_safe_default_kx_groups()
            .with_protocol_versions(TLS_VERSIONS)
            .expect("server config must be valid")
            .with_client_cert_verifier(client_cert_verifier)
            .with_cert_resolver(Arc::new(resolver))
    }

    /// Ensures the certificate is valid for the services we terminate for TLS. This assumes that
    /// server cert validation does the same or more validation than client cert validation.
    fn validate(&self, client: &rustls::ClientConfig, certs: &[rustls::Certificate]) -> Result<()> {
        let name = rustls::ServerName::try_from(self.name.as_str())
            .expect("server name must be a valid DNS name");
        static NO_OCSP: &[u8] = &[];
        let end_entity = &certs[0];
        let intermediates = &certs[1..];
        self.server_cert_verifier.verify_server_cert(
            end_entity,
            intermediates,
            &name,
            &mut std::iter::empty(), // no certificate transparency logs
            NO_OCSP,
            std::time::SystemTime::now(),
        )?;
        debug!("Certified");
        Ok(())
    }
}

impl id::Credentials for Store {
    /// Returns the proxy's identity.
    fn dns_name(&self) -> &id::Name {
        &self.name
    }

    /// Returns the CSR that was configured at proxy startup.
    fn gen_certificate_signing_request(&mut self) -> id::DerX509 {
        id::DerX509(self.csr.to_vec())
    }

    /// Publishes TLS client and server configurations using
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

        let resolver = CertResolver(Arc::new(rustls::sign::CertifiedKey::new(
            chain.clone(),
            Arc::new(Key(self.key.clone())),
        )));

        // Build new client and server TLS configs.
        let client = self.client(resolver.clone());
        let server = self.server(resolver);

        // Use the client's verifier to validate the certificate for our local name.
        self.validate(&client, &*chain)?;

        // Publish the new configs.
        let _ = self.client_tx.send(client.into());
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
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rustls::Error> {
        let rng = rand::SystemRandom::new();
        self.0
            .sign(&rng, message)
            .map(|signature| signature.as_ref().to_owned())
            .map_err(|ring::error::Unspecified| rustls::Error::General("Signing Failed".to_owned()))
    }

    fn scheme(&self) -> rustls::SignatureScheme {
        SIGNATURE_ALG_RUSTLS_SCHEME
    }
}

// === impl CertResolver ===

impl CertResolver {
    #[inline]
    fn resolve_(
        &self,
        sigschemes: &[rustls::SignatureScheme],
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        if !sigschemes.contains(&SIGNATURE_ALG_RUSTLS_SCHEME) {
            debug!("Signature scheme not supported -> no certificate");
            return None;
        }

        Some(self.0.clone())
    }
}

impl rustls::client::ResolvesClientCert for CertResolver {
    fn resolve(
        &self,
        _acceptable_issuers: &[&[u8]],
        sigschemes: &[rustls::SignatureScheme],
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        self.resolve_(sigschemes)
    }

    fn has_certs(&self) -> bool {
        true
    }
}

impl rustls::server::ResolvesServerCert for CertResolver {
    fn resolve(
        &self,
        hello: rustls::server::ClientHello<'_>,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        let server_name = match hello.server_name() {
            Some(name) => webpki::DnsNameRef::try_from_ascii_str(name)
                .expect("server name must be a valid server name"),

            None => {
                debug!("no SNI -> no certificate");
                return None;
            }
        };

        // Verify that our certificate is valid for the given SNI name.
        let c = self.0.cert.first()?;
        if let Err(error) = webpki::EndEntityCert::try_from(c.as_ref())
            .and_then(|c| c.verify_is_valid_for_dns_name(server_name))
        {
            debug!(%error, "Local certificate is not valid for SNI");
            return None;
        };

        self.resolve_(hello.signature_schemes())
    }
}
