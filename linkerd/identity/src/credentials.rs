use linkerd_error::Result;
use std::ops::Deref;

/// Publishes certificates to be used by TLS implementations.
pub trait Credentials {
    /// Set the certificate returned by the identity service.
    ///
    /// Fails if the certificate is not valid.
    fn set_certificate(&mut self, leaf: DerX509, chain: Vec<DerX509>, key: Vec<u8>) -> Result<()>;
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
        &self.0
    }
}
