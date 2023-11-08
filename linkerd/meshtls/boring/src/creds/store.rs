use super::{verify, BaseCreds, Certs, Creds, CredsTx};
use boring::x509::{X509StoreContext, X509};
use linkerd_error::Result;
use linkerd_identity as id;
use std::sync::Arc;

pub struct Store {
    creds: Arc<BaseCreds>,
    csr: Vec<u8>,
    id: id::Id,
    tx: CredsTx,
}

// === impl Store ===

impl Store {
    pub(super) fn new(creds: Arc<BaseCreds>, csr: &[u8], id: id::Id, tx: CredsTx) -> Self {
        Self {
            creds,
            csr: csr.into(),
            id,
            tx,
        }
    }
}

impl id::Credentials for Store {
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

        verify::verify_id(&leaf, &self.id)?;

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
