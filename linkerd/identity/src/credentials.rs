use crate::Name;
use linkerd_error::Result;
use std::{ops::Deref, time::SystemTime};

/// Publishes certificates to be used by TLS implementations.
pub trait Credentials {
    /// Get the authoritative DNS-like name used in the certificate.
    fn dns_name(&self) -> &Name;

    /// Generate a CSR to to be sent to the identity service.
    fn gen_certificate_signing_request(&mut self) -> DerX509;

    /// Set the certificate returned by the identity service.
    ///
    /// Fails if the certificate is not valid.
    fn set_certificate(
        &mut self,
        leaf: DerX509,
        chain: Vec<DerX509>,
        expiry: SystemTime,
    ) -> Result<()>;
}

/// DER-formatted X.509 data.
#[derive(Clone, Debug)]
pub struct DerX509(pub Vec<u8>);

// === impl DerX509 ===

impl DerX509 {
    pub fn to_vec(&self) -> Vec<u8> {
        self.0.clone()
    }
}

impl Deref for DerX509 {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}
