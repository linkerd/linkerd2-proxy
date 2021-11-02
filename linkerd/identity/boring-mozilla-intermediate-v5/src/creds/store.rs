use boring::{
    pkey::{PKey, Private},
    ssl,
    x509::{store::X509Store, X509StoreContext, X509},
};
use linkerd_error::Result;
use linkerd_identity as id;
use tokio::sync::watch;

pub struct Store {
    roots: X509Store,
    key: PKey<Private>,
    csr: Vec<u8>,
    name: id::Name,
    client_tx: watch::Sender<ssl::SslConnector>,
    server_tx: watch::Sender<ssl::SslAcceptor>,
}

// === impl Store ===

impl Store {
    pub(super) fn new(
        roots: X509Store,
        key: PKey<Private>,
        csr: &[u8],
        name: id::Name,
        client_tx: watch::Sender<ssl::SslConnector>,
        server_tx: watch::Sender<ssl::SslAcceptor>,
    ) -> Self {
        Self {
            roots,
            key,
            csr: csr.into(),
            name,
            client_tx,
            server_tx,
        }
    }

    fn cert_matches_name(&self, cert: &X509) -> bool {
        for san in cert.subject_alt_names().into_iter().flatten() {
            if let Some(n) = san.dnsname() {
                if let Ok(name) = n.parse::<linkerd_dns_name::Name>() {
                    if name == *self.name {
                        return true;
                    }
                }
            }
        }

        false
    }

    fn clone_roots(&self) -> Result<X509Store> {
        // X509Store does not implement clone, so we need to manually copy it.
        let mut roots = boring::x509::store::X509StoreBuilder::new()?;
        for obj in self.roots.objects() {
            if let Some(c) = obj.x509() {
                roots.add_cert(c.to_owned())?;
            }
        }
        Ok(roots.build())
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
        let cert = X509::from_der(&leaf)?;
        if !self.cert_matches_name(&cert) {
            return Err("certificate does not have a DNS name SAN for the local identity".into());
        }

        let mut chain = boring::stack::Stack::new()?;
        chain.push(cert.clone())?;
        for id::DerX509(der) in intermediates.iter() {
            let cert = X509::from_der(der)?;
            chain.push(cert)?;
        }

        let mut context = X509StoreContext::new()?;
        if !context.init(&self.roots, &cert, &chain, |c| c.verify_cert())? {
            return Err("certificate could not be validated against the trust chain".into());
        }

        let conn = {
            // FIXME(ver) Restrict TLS version, algorithms, etc.
            let mut b = ssl::SslConnector::builder(ssl::SslMethod::tls_client())?;
            b.set_private_key(self.key.as_ref())?;
            b.set_cert_store(self.clone_roots()?);
            b.set_certificate(cert.as_ref())?;
            for id::DerX509(der) in intermediates.iter() {
                let cert = X509::from_der(der)?;
                b.add_extra_chain_cert(cert)?;
            }
            b.build()
        };

        let acc = {
            // mozilla_intermediate_v5 is the only variant that enables TLSv1.3, so we use that.
            // TODO(ver) Ensure that this configuration includes FIPS-approved algorithms.
            let mut b = ssl::SslAcceptor::mozilla_intermediate_v5(ssl::SslMethod::tls_server())?;
            b.set_private_key(self.key.as_ref())?;
            b.set_cert_store(self.clone_roots()?);
            b.set_certificate(cert.as_ref())?;
            for id::DerX509(der) in intermediates.iter() {
                let cert = X509::from_der(der)?;
                b.add_extra_chain_cert(cert)?;
            }
            b.set_verify(ssl::SslVerifyMode::PEER);
            b.check_private_key()?;
            b.build()
        };

        let _ = self.server_tx.send(acc);
        let _ = self.client_tx.send(conn);

        Ok(())
    }
}
