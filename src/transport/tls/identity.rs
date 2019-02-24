use super::{webpki, DnsName, InvalidDnsName};
use api;
use convert::TryFrom;
use std::fmt;
use std::sync::Arc;

/// An endpoint's identity.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct Identity(pub(super) Arc<DnsName>);

impl Identity {
    /// Parses the given TLS identity, if provided.
    ///
    /// In the event of an error, the error is logged, so no detailed error
    /// information is returned.
    pub fn maybe_from_protobuf(
        pb: api::destination::TlsIdentity,
    ) -> Result<Option<Self>, ()> {
        use api::destination::tls_identity::Strategy;
        match pb.strategy {
            Some(Strategy::K8sPodIdentity(i)) => {
                Self::from_sni_hostname(i.pod_identity.as_bytes()).map(Some)
            }
            None => Ok(None), // No TLS.
        }
    }

    pub fn from_sni_hostname(hostname: &[u8]) -> Result<Self, ()> {
        if hostname.last() == Some(&b'.') {
            return Err(()); // SNI hostnames are implicitly absolute.
        }
        DnsName::try_from(hostname)
            .map(|name| Identity(Arc::new(name)))
            .map_err(|InvalidDnsName| {
                error!("Invalid DNS name: {:?}", hostname);
                ()
            })
    }

    pub fn from_client_cert(name: &webpki::DNSNameRef) -> Self {
        Identity(Arc::new(DnsName(name.to_owned())))
    }

    pub(super) fn as_dns_name_ref(&self) -> webpki::DNSNameRef {
        (self.0).0.as_ref()
    }
}

impl AsRef<str> for Identity {
    fn as_ref(&self) -> &str {
        (*self.0).as_ref()
    }
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        self.as_ref().fmt(f)
    }
}
