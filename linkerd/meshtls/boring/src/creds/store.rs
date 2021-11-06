use super::{BaseCreds, Certs, Creds, CredsTx};
use boring::x509::{X509StoreContext, X509};
use linkerd_error::Result;
use linkerd_identity as id;
use std::sync::Arc;

pub struct Store {
    creds: Arc<BaseCreds>,
    csr: Vec<u8>,
    name: id::Name,
    tx: CredsTx,
}

// === impl Store ===

impl Store {
    pub(super) fn new(creds: Arc<BaseCreds>, csr: &[u8], name: id::Name, tx: CredsTx) -> Self {
        Self {
            creds,
            csr: csr.into(),
            name,
            tx,
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
        let leaf = X509::from_der(&leaf)?;
        if !self.cert_matches_name(&leaf) {
            return Err("certificate does not have a DNS name SAN for the local identity".into());
        }

        let intermediates = intermediates
            .into_iter()
            .map(|id::DerX509(der)| X509::from_der(&der).map_err(Into::into))
            .collect::<Result<Vec<_>>>()?;

        let creds = Creds {
            base: self.creds.clone(),
            certs: Some(Certs {
                leaf,
                intermediates,
            }),
        };

        let mut context = X509StoreContext::new()?;
        let roots = creds.root_store()?;

        let mut chain = boring::stack::Stack::new()?;
        for i in &creds.certs.as_ref().unwrap().intermediates {
            chain.push(i.to_owned())?;
        }
        let init = {
            let leaf = &creds.certs.as_ref().unwrap().leaf;
            context.init(&roots, leaf, &chain, |c| c.verify_cert())?
        };
        if !init {
            return Err("certificate could not be validated against the trust chain".into());
        }

        // If receivers are dropped, we don't return an error (as this would likely cause the
        // updater to retry more aggressively). It's fine to silently ignore these errors.
        let _ = self.tx.send(creds);

        Ok(())
    }
}
