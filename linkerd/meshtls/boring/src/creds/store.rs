use super::{BaseCreds, Certs, Creds, CredsTx};
use boring::pkey::PKey;
use boring::x509::{X509StoreContext, X509};
use linkerd_error::Result;
use linkerd_identity as id;
use linkerd_meshtls_verifier as verifier;
use std::sync::Arc;

pub struct Store {
    creds: Arc<BaseCreds>,
    id: id::Id,
    tx: CredsTx,
}

// === impl Store ===

impl Store {
    pub(super) fn new(creds: Arc<BaseCreds>, id: id::Id, tx: CredsTx) -> Self {
        Self { creds, id, tx }
    }
}

impl id::Credentials for Store {
    /// Publishes TLS client and server configurations using
    fn set_certificate(
        &mut self,
        id::DerX509(leaf_der): id::DerX509,
        intermediates: Vec<id::DerX509>,
        key_pkcs8: Vec<u8>,
    ) -> Result<()> {
        let leaf = X509::from_der(&leaf_der)?;

        verifier::verify_id(&leaf_der, &self.id)?;

        let intermediates = intermediates
            .into_iter()
            .map(|id::DerX509(der)| X509::from_der(&der).map_err(Into::into))
            .collect::<Result<Vec<_>>>()?;

        let key = PKey::private_key_from_pkcs8(&key_pkcs8)?;
        let creds = Creds {
            base: self.creds.clone(),
            certs: Some(Certs {
                leaf,
                intermediates,
                key,
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
