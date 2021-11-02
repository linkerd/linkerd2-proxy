use boring::{
    error::ErrorStack,
    pkey::{PKey, Private},
    x509::{store::X509Store, X509},
};
use linkerd_error::Result;
use linkerd_identity as id;

pub struct Store {
    roots: X509Store,
    key: PKey<Private>,
    csr: Vec<u8>,
    name: id::Name,
}

// === impl Store ===

impl Store {
    pub(super) fn new(
        roots: X509Store,
        key: PKey<Private>,
        csr: &[u8],
        name: id::Name,
        // client_tx: watch::Sender<Arc<rustls::ClientConfig>>,
        // server_tx: watch::Sender<Arc<rustls::ServerConfig>>,
    ) -> Self {
        Self {
            roots,
            key,
            csr: csr.into(),
            name,
            // client_tx,
            // server_tx,
        }
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
        let cert = X509::from_der(&leaf)?;
        chain.push(cert);
        chain.extend(
            intermediates
                .into_iter()
                .map(|crt| X509::from_der(&crt))
                .collect::<Result<Vec<_>, ErrorStack>>()?,
        );

        Ok(())
    }
}
