use linkerd_error::Result;
use linkerd_proxy_identity as id;
use ring::{error::KeyRejected, rand, signature::EcdsaKeyPair};
use std::{sync::Arc, time::SystemTime};
use thiserror::Error;
use tokio::sync::watch;
use tokio_rustls::rustls::{self};
use tracing::{debug, warn};

pub struct Store {
    roots: rustls::RootCertStore,
    key: Arc<EcdsaKeyPair>,
    csr: Arc<[u8]>,
    identity: id::Name,
    client_tx: watch::Sender<Arc<rustls::ClientConfig>>,
    server_tx: watch::Sender<Option<Arc<rustls::ServerConfig>>>,
}

#[derive(Clone)]
pub struct Receiver {
    client_rx: watch::Receiver<Arc<rustls::ClientConfig>>,
    server_rx: watch::Receiver<Option<Arc<rustls::ServerConfig>>>,
}

#[derive(Debug, Error)]
#[error(transparent)]
pub struct InvalidKey(KeyRejected);

#[derive(Debug, Error)]
#[error("invalid trust roots")]
pub struct InvalidTrustRoots(());

#[derive(Debug, Error)]
#[error("credential store lost")]
pub struct LostStore(());

#[derive(Clone)]
struct Key(Arc<EcdsaKeyPair>);

struct CertResolver(rustls::sign::CertifiedKey);

// These must be kept in sync:
static SIGNATURE_ALG_RING_SIGNING: &ring::signature::EcdsaSigningAlgorithm =
    &ring::signature::ECDSA_P256_SHA256_ASN1_SIGNING;
const SIGNATURE_ALG_RUSTLS_SCHEME: rustls::SignatureScheme =
    rustls::SignatureScheme::ECDSA_NISTP256_SHA256;
const SIGNATURE_ALG_RUSTLS_ALGORITHM: rustls::internal::msgs::enums::SignatureAlgorithm =
    rustls::internal::msgs::enums::SignatureAlgorithm::ECDSA;
const TLS_VERSIONS: &[rustls::ProtocolVersion] = &[rustls::ProtocolVersion::TLSv1_3];
static TLS_SUPPORTED_CIPHERSUITES: [&rustls::SupportedCipherSuite; 1] =
    [&rustls::ciphersuite::TLS13_CHACHA20_POLY1305_SHA256];

pub fn watch(
    identity: id::Name,
    roots_pem: &str,
    key_pkcs8: &[u8],
    csr: &[u8],
) -> Result<(Store, Receiver)> {
    let mut roots = rustls::RootCertStore::empty();
    let (added, skipped) = roots
        .add_pem_file(&mut std::io::Cursor::new(roots_pem))
        .map_err(InvalidTrustRoots)?;
    if skipped != 0 {
        warn!("Skipped {} invalid trust anchors", skipped);
    }
    if added == 0 {
        return Err("no trust roots loaded".into());
    }

    let key =
        EcdsaKeyPair::from_pkcs8(SIGNATURE_ALG_RING_SIGNING, key_pkcs8).map_err(InvalidKey)?;

    let (client_tx, client_rx) = watch::channel(Arc::new(rustls::ClientConfig::new()));
    let (server_tx, server_rx) = watch::channel(None);

    let store = Store {
        roots,
        key: Arc::new(key),
        csr: csr.into(),
        identity,
        client_tx,
        server_tx,
    };
    let rx = Receiver {
        client_rx,
        server_rx,
    };

    Ok((store, rx))
}

impl id::Credentials for Store {
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
        _expiry: SystemTime,
    ) -> Result<()> {
        let mut chain = Vec::with_capacity(intermediates.len() + 1);
        chain.push(rustls::Certificate(leaf));
        chain.extend(intermediates.into_iter().map(rustls::Certificate));

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

        let _ = self.server_tx.send(Some(server.into()));

        Ok(())
    }
}

// === impl Receiver ===

impl Receiver {
    // pub fn client(&self) -> crate::Connect {
    //     crate::Connect::new(self.client_rx.clone())
    // }

    pub async fn spawn_server(
        &mut self,
        alpn_protocols: Vec<Vec<u8>>,
    ) -> Result<crate::Terminate, LostStore> {
        let mut orig_rx = self.server_rx.clone();

        let mut config = Self::wait_for_server(&mut orig_rx).await?;

        // If we're creating a server that advertises ALPN, we create a new Arc<ServerConfig> that
        // includes the relevant ALPN protocols. This allows servers that expose ALPN to avoid
        // cloning the server config for every connection by, instead, doing this clone at the watch
        // level.
        if !alpn_protocols.is_empty() {
            let mut c = (*config).clone();
            c.alpn_protocols = alpn_protocols.clone();
            config = c.into();
        }

        let (tx, rx) = watch::channel(config);

        // Spawn a background task that watches the optional server configuration and publishes it
        // as a reliable channel, including any ALPN overrides.
        let task = tokio::spawn(async move {
            loop {
                if orig_rx.changed().await.is_err() {
                    return;
                }

                if let Some(mut config) = orig_rx.borrow().as_ref().cloned() {
                    // If there's an ALPN override, copy the config with the new alpn values.
                    if !alpn_protocols.is_empty() {
                        let mut c = (*config).clone();
                        c.alpn_protocols = alpn_protocols.clone();
                        config = c.into();
                    }

                    if tx.send(config).is_err() {
                        return;
                    }
                }
            }
        });

        Ok(crate::Terminate::new(rx, task))
    }

    /// Waits until a server TLS config is ready.
    async fn wait_for_server(
        rx: &mut watch::Receiver<Option<Arc<rustls::ServerConfig>>>,
    ) -> Result<Arc<rustls::ServerConfig>, LostStore> {
        loop {
            let config = (*rx.borrow_and_update()).as_ref().cloned();
            if let Some(c) = config {
                return Ok(c);
            }

            rx.changed().await.map_err(|_| LostStore(()))?;
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time;

    #[tokio::test]
    async fn test_spawn_server() {
        time::pause();

        let (server_tx, server_rx) = watch::channel(None);
        let (_, client_rx) = watch::channel(Arc::new(rustls::ClientConfig::new()));
        let receiver = Receiver {
            server_rx,
            client_rx,
        };

        let mut server = tokio::spawn({
            let mut rx = receiver.clone();
            async move {
                rx.spawn_server(vec![])
                    .await
                    .expect("sender must not be lost")
            }
        });

        tokio::select! {
            _ = (&mut server) => panic!("server must not be ready"),
            _ = time::sleep(time::Duration::from_millis(100)) => {}
        }

        let server_config = Arc::new(rustls::ServerConfig::new(rustls::NoClientAuth::new()));
        server_tx
            .send(Some(server_config.clone()))
            .ok()
            .expect("receiver is held");

        tokio::select! {
            res = (&mut server) => {
                let srv = res.expect("task must complete");
                // Test that we're using the same exact Arc.
                assert!(Arc::ptr_eq(&server_config, &srv.config()));
            }
            _ = time::sleep(time::Duration::from_millis(100)) => {
                panic!("server must be ready");
            }
        }
    }

    #[tokio::test]
    async fn test_spawn_server_alpn() {
        time::pause();

        let (server_tx, server_rx) = watch::channel(None);
        let (_, client_rx) = watch::channel(Arc::new(rustls::ClientConfig::new()));
        let receiver = Receiver {
            server_rx,
            client_rx,
        };

        let mut server = tokio::spawn({
            let mut rx = receiver.clone();
            async move {
                rx.spawn_server(vec![b"my.alpn".to_vec()])
                    .await
                    .expect("sender must not be lost")
            }
        });

        let server_config = Arc::new(rustls::ServerConfig::new(rustls::NoClientAuth::new()));
        server_tx
            .send(Some(server_config.clone()))
            .ok()
            .expect("receiver is held");

        tokio::select! {
            res = (&mut server) => {
                let srv = res.expect("task must complete");
                let sc = srv.config();
                // Confirm that we're not using the same exact Arc, since we're using a copy with
                // ALPN set.
                assert!(!Arc::ptr_eq(&server_config, &sc));
                assert_eq!(sc.alpn_protocols, [b"my.alpn"]);
            }
            _ = time::sleep(time::Duration::from_millis(100)) => {
                panic!("server must be ready");
            }
        }
    }
}
