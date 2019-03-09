use convert::TryFrom;
use dns;
use std::fmt;

/// An endpoint's identity.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct Identity(dns::Name);

impl From<dns::Name> for Identity {
    fn from(n: dns::Name) -> Self {
        Identity(n)
    }
}

impl Identity {
    pub fn from_sni_hostname(hostname: &[u8]) -> Result<Self, dns::InvalidName> {
        if hostname.last() == Some(&b'.') {
            return Err(dns::InvalidName); // SNI hostnames are implicitly absolute.
        }

        dns::Name::try_from(hostname).map(|n| n.into())
    }

    pub fn as_dns_name_ref(&self) -> webpki::DNSNameRef {
        self.0.as_dns_name_ref()
    }
}

impl AsRef<str> for Identity {
    fn as_ref(&self) -> &str {
        AsRef::<str>::as_ref(&self.0)
    }
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        fmt::Debug::fmt(&self.0, f)
    }
}
