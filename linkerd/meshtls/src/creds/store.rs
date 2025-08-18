use super::{default_provider, params::*};
use linkerd_dns_name as dns;
use linkerd_error::Result;
use linkerd_identity as id;
use linkerd_meshtls_verifier as verifier;
use std::{convert::TryFrom, sync::Arc};
use tokio::sync::watch;
use tokio_rustls::rustls::{
    self,
    pki_types::{PrivatePkcs8KeyDer, UnixTime},
    server::WebPkiClientVerifier,
    sign::CertifiedKey,
};
use tracing::debug;

pub struct Store {
    roots: rustls::RootCertStore,
    server_cert_verifier: Arc<dyn rustls::client::danger::ServerCertVerifier>,
    server_id: id::Id,
    server_name: dns::Name,
    client_tx: watch::Sender<Arc<rustls::ClientConfig>>,
    server_tx: watch::Sender<Arc<rustls::ServerConfig>>,
}

#[derive(Clone, Debug)]
struct CertResolver(Arc<rustls::sign::CertifiedKey>);

pub(super) fn client_config_builder(
    cert_verifier: Arc<dyn rustls::client::danger::ServerCertVerifier>,
) -> rustls::ConfigBuilder<rustls::ClientConfig, rustls::client::WantsClientCert> {
    rustls::ClientConfig::builder_with_provider(default_provider())
        .with_protocol_versions(TLS_VERSIONS)
        .expect("client config must be valid")
        // XXX: Rustls's built-in verifiers don't let us tweak things as fully
        // as we'd like (e.g. controlling the set of trusted signature
        // algorithms), but they provide good enough defaults for now.
        // TODO: lock down the verification further.
        //
        // NOTE(eliza): Rustls considers setting a custom server cert verifier
        // to be a "dangerous configuration", but we're doing *exactly* what its
        // builder API does internally. However, we want to share the verifier
        // with the `Store` so that it can be used in `Store::validate` which
        // requires using this API.
        .dangerous()
        .with_custom_certificate_verifier(cert_verifier)
}

pub(super) fn server_config(
    roots: rustls::RootCertStore,
    resolver: Arc<dyn rustls::server::ResolvesServerCert>,
) -> Arc<rustls::ServerConfig> {
    // Ask TLS clients for a certificate and accept any certificate issued by our trusted CA(s).
    //
    // XXX: Rustls's built-in verifiers don't let us tweak things as fully as we'd like (e.g.
    // controlling the set of trusted signature algorithms), but they provide good enough
    // defaults for now.
    // TODO: lock down the verification further.
    let provider = default_provider();

    let client_cert_verifier =
        WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
            .allow_unauthenticated()
            .build()
            .expect("server verifier must be valid");

    rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(TLS_VERSIONS)
        .expect("server config must be valid")
        .with_client_cert_verifier(client_cert_verifier)
        .with_cert_resolver(resolver)
        .into()
}

// === impl Store ===

impl Store {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        roots: rustls::RootCertStore,
        server_cert_verifier: Arc<dyn rustls::client::danger::ServerCertVerifier>,
        server_id: id::Id,
        server_name: dns::Name,
        client_tx: watch::Sender<Arc<rustls::ClientConfig>>,
        server_tx: watch::Sender<Arc<rustls::ServerConfig>>,
    ) -> Self {
        Self {
            roots,
            server_cert_verifier,
            server_id,
            server_name,
            client_tx,
            server_tx,
        }
    }

    /// Builds a new TLS client configuration.
    fn client_config(&self, resolver: Arc<CertResolver>) -> Arc<rustls::ClientConfig> {
        let mut cfg = client_config_builder(self.server_cert_verifier.clone())
            .with_client_cert_resolver(resolver);

        // Disable session resumption for the time-being until resumption is
        // more tested.
        cfg.resumption = rustls::client::Resumption::disabled();

        cfg.into()
    }

    /// Ensures the certificate is valid for the services we terminate for TLS. This assumes that
    /// server cert validation does the same or more validation than client cert validation.
    fn validate(&self, certs: &[rustls::pki_types::CertificateDer<'_>]) -> Result<()> {
        let name = rustls::pki_types::ServerName::try_from(self.server_name.as_str())
            .expect("server name must be a valid DNS name");
        static NO_OCSP: &[u8] = &[];
        let end_entity = &certs[0];
        let intermediates = &certs[1..];
        let now = UnixTime::now();
        self.server_cert_verifier.verify_server_cert(
            end_entity,
            intermediates,
            &name,
            NO_OCSP,
            now,
        )?;

        // verify the id as the cert verifier does not do that (on purpose)
        verifier::verify_id(end_entity, &self.server_id).map_err(Into::into)
    }
}
impl id::Credentials for Store {
    /// Publishes TLS client and server configurations using
    fn set_certificate(
        &mut self,
        id::DerX509(leaf): id::DerX509,
        intermediates: Vec<id::DerX509>,
        key: Vec<u8>,
        _expiry: std::time::SystemTime,
    ) -> Result<()> {
        let mut chain = Vec::with_capacity(intermediates.len() + 1);
        chain.push(rustls::pki_types::CertificateDer::from(leaf));
        chain.extend(
            intermediates
                .into_iter()
                .map(|id::DerX509(der)| rustls::pki_types::CertificateDer::from(der)),
        );

        // Use the client's verifier to validate the certificate for our local name.
        self.validate(&chain)?;

        let key_der = PrivatePkcs8KeyDer::from(key);
        let provider = rustls::crypto::CryptoProvider::get_default()
            .expect("Failed to get default crypto provider");
        let key = CertifiedKey::from_der(chain, key_der.into(), provider)?;
        let resolver = Arc::new(CertResolver(Arc::new(key)));

        // Build new client and server TLS configs.
        let client = self.client_config(resolver.clone());
        let server = server_config(self.roots.clone(), resolver);

        // Publish the new configs.
        let _ = self.client_tx.send(client);
        let _ = self.server_tx.send(server);

        Ok(())
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
        self.resolve_(hello.signature_schemes())
    }
}
