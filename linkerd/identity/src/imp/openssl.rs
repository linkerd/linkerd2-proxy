use std::fmt;
use std::sync::Arc;
use std::time::SystemTime;

#[cfg(feature = "boring-tls")]
use boring::{
    error::ErrorStack,
    pkey::{PKey, Private},
    stack::Stack,
    x509::{
        store::{X509Store, X509StoreBuilder},
        {X509StoreContext, X509VerifyResult, X509},
    },
};
#[cfg(not(feature = "boring-tls"))]
use openssl::{
    error::ErrorStack,
    pkey::{PKey, Private},
    stack::Stack,
    x509::{
        store::{X509Store, X509StoreBuilder},
        {X509StoreContext, X509VerifyResult, X509},
    },
};

use tracing::{debug, error};

use crate::{LocalId, Name};
use std::fmt::Formatter;
use thiserror::Error;

#[derive(Clone, Debug)]
pub struct Key(pub Arc<PKey<Private>>);

impl Key {
    pub fn from_pkcs8(b: &[u8]) -> Result<Key, Error> {
        let key = PKey::private_key_from_pkcs8(b)?;
        Ok(Key(Arc::new(key)))
    }
}

#[derive(Clone, Debug, Error)]
#[error(transparent)]
pub struct Error(#[from] ErrorStack);

#[derive(Clone)]
pub struct TrustAnchors(Arc<X509Store>);

impl TrustAnchors {
    fn store() -> X509StoreBuilder {
        X509StoreBuilder::new().expect("unable to create certificate store")
    }

    #[cfg(any(test, feature = "test-util"))]
    pub fn empty() -> Self {
        Self(Arc::new(TrustAnchors::store().build()))
    }

    pub fn from_pem(s: &str) -> Option<Self> {
        debug!("Loading {} into x509", s);

        return match X509::from_pem(s.as_bytes()) {
            Ok(cert) => {
                let mut store = TrustAnchors::store();
                debug!("Adding trust {:?}", cert);
                if store.add_cert(cert).is_err() {
                    error!("unable to add certificate to trust anchors");
                    return None
                }

                Some(Self(Arc::new(store.build())))
            }
            Err(err) => {
                error!("unable to construct trust anchor {}", err);
                None
            },
        }
    }

    pub fn certify(&self, key: Key, crt: Crt) -> Result<CrtKey, InvalidCrt> {
        let cert = crt.cert.clone();
        if cert
            .subject_alt_names()
            .unwrap()
            .iter()
            .filter(|n| n.dnsname().expect("unable to convert to dns name") == crt.name().as_ref())
            .next()
            .is_none()
        {
            return Err(InvalidCrt::SubjectAltName(crt.id));
        }

        let mut chain = Stack::new()?;
        chain.push(cert.clone())?;
        for chain_crt in crt.chain.clone() {
            chain.push(chain_crt)?;
        }

        let mut context = X509StoreContext::new()?;
        context.init(&self.0, &cert, &chain, |c| match c.verify_cert() {
            Ok(true) => Ok(Ok(true)),
            Ok(false) => Ok(Err(InvalidCrt::Verify(c.error()))),
            Err(err) => Err(err),
        })??;

        let server_config =
            ServerConfig::new(vec![], self.0.clone(), Some(crt.clone()), Some(key.clone()));
        let client_config =
            ClientConfig::new(vec![], self.0.clone(), Some(crt.clone()), Some(key.clone()));

        Ok(CrtKey {
            id: crt.id.clone(),
            expiry: crt.expiry.clone(),
            client_config: Arc::new(client_config),
            server_config: Arc::new(server_config),
        })
    }

    pub fn client_config(&self) -> Arc<ClientConfig> {
        Arc::new(ClientConfig::new(vec![], self.0.clone(), None, None))
    }
}

impl fmt::Debug for TrustAnchors {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.pad("openssl::TrustAnchors")
    }
}

#[derive(Clone, Debug, Error)]
pub enum InvalidCrt {
    #[error("subject alt name incorrect ({0})")]
    SubjectAltName(LocalId),
    #[error("{0}")]
    Verify(#[source] X509VerifyResult),
    #[error(transparent)]
    General(#[from] Error),
}

impl From<ErrorStack> for InvalidCrt {
    fn from(err: ErrorStack) -> Self {
        InvalidCrt::General(err.into())
    }
}

#[derive(Clone)]
pub struct CrtKey {
    id: LocalId,
    expiry: SystemTime,
    client_config: Arc<ClientConfig>,
    server_config: Arc<ServerConfig>,
}

// === CrtKey ===
impl CrtKey {
    pub fn name(&self) -> &Name {
        self.id.as_ref()
    }

    pub fn expiry(&self) -> SystemTime {
        self.expiry
    }

    pub fn id(&self) -> &LocalId {
        &self.id
    }

    pub fn client_config(&self) -> Arc<ClientConfig> {
        self.client_config.clone()
    }

    pub fn server_config(&self) -> Arc<ServerConfig> {
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

#[derive(Clone, Debug)]
pub struct Crt {
    pub(crate) id: LocalId,
    expiry: SystemTime,
    pub cert: X509,
    pub chain: Vec<X509>,
}

impl Crt {
    pub fn new(
        id: LocalId,
        leaf: Vec<u8>,
        intermediates: Vec<Vec<u8>>,
        expiry: SystemTime,
    ) -> Self {
        let mut chain = Vec::with_capacity(intermediates.len() + 1);
        let cert = X509::from_der(&leaf).expect("unable to convert to a x509 certificate");
        chain.extend(
            intermediates
                .into_iter()
                .map(|crt| X509::from_der(&crt).expect("unable to add intermediate certificate")),
        );

        Self {
            id,
            cert,
            chain,
            expiry,
        }
    }

    pub fn name(&self) -> &Name {
        self.id.as_ref()
    }
}

#[derive(Clone)]
pub struct ClientConfig {
    pub root_certs: Arc<X509Store>,
    pub key: Option<Arc<Key>>,
    pub cert: Option<Arc<Crt>>,
    protocols: Arc<Vec<Vec<u8>>>,
}

impl fmt::Debug for ClientConfig {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientConfig")
            .field("protocols", &self.protocols)
            .finish()
    }
}

impl ClientConfig {
    pub fn new(
        protocols: Vec<Vec<u8>>,
        root_certs: Arc<X509Store>,
        cert: Option<Crt>,
        key: Option<Key>,
    ) -> Self {
        Self {
            root_certs,
            protocols: Arc::new(protocols),
            key: key.map(Arc::new),
            cert: cert.map(Arc::new),
        }
    }
    pub fn empty() -> Self {
        ClientConfig::new(
            Vec::new(),
            Arc::new(X509StoreBuilder::new().expect("unable to construct root certs").build()),
            None,
            None,
        )
    }

    pub fn set_protocols(&mut self, protocols: Vec<Vec<u8>>) {
        self.protocols = Arc::new(protocols)
    }
}

#[derive(Clone)]
pub struct ServerConfig {
    pub root_certs: Arc<X509Store>,
    pub key: Option<Arc<Key>>,
    pub cert: Option<Arc<Crt>>,
    alpn_protocols: Arc<Vec<Vec<u8>>>,
}

impl ServerConfig {
    pub fn new(
        alpn_protocols: Vec<Vec<u8>>,
        root_certs: Arc<X509Store>,
        cert: Option<Crt>,
        key: Option<Key>,
    ) -> Self {
        Self {
            alpn_protocols: Arc::new(alpn_protocols),
            root_certs,
            key: key.map(Arc::new),
            cert: cert.map(Arc::new),
        }
    }
    /// Produces a server config that fails to handshake all connections.
    pub fn empty() -> Self {
        ServerConfig::new(
            Vec::new(),
            Arc::new(X509StoreBuilder::new().expect("unable to construct root certs").build()),
            None,
            None,
        )
    }

    pub fn add_protocols(&mut self, protocols: Vec<u8>) {
        self.alpn_protocols.as_ref().clone().push(protocols)
    }
}

impl fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerConfig")
            .field("alpn_protocols", &self.alpn_protocols)
            .field("key", &self.key)
            .finish()
    }
}
