use linkerd_dns_name::InvalidName;
use std::{fmt, ops::Deref, str::FromStr, sync::Arc};

/// An endpoint's identity.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct TlsName(pub Arc<linkerd_dns_name::Name>);

// === impl Name ===

impl From<linkerd_dns_name::Name> for TlsName {
    fn from(n: linkerd_dns_name::Name) -> Self {
        TlsName(Arc::new(n))
    }
}

impl FromStr for TlsName {
    type Err = InvalidName;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.ends_with('.') {
            return Err(InvalidName); // SNI hostnames are implicitly absolute.
        }

        linkerd_dns_name::Name::from_str(s).map(|n| TlsName(Arc::new(n)))
    }
}

impl Deref for TlsName {
    type Target = linkerd_dns_name::Name;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Debug for TlsName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl fmt::Display for TlsName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        fmt::Display::fmt(&self.0, f)
    }
}
