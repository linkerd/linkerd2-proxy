#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]

pub use linkerd_dns_name::InvalidName;
use std::{convert::TryFrom, fmt, str::FromStr, sync::Arc};

/// An endpoint's identity.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct Name(Arc<linkerd_dns_name::Name>);

/// A newtype for local server identities.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct LocalId(pub Name);

// === impl Name ===

impl From<linkerd_dns_name::Name> for Name {
    fn from(n: linkerd_dns_name::Name) -> Self {
        Name(Arc::new(n))
    }
}

impl std::ops::Deref for Name {
    type Target = linkerd_dns_name::Name;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Name {
    #[inline]
    pub fn as_webpki(&self) -> webpki::DNSNameRef<'_> {
        self.0.as_webpki()
    }
}

impl FromStr for Name {
    type Err = InvalidName;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.as_bytes().last() == Some(&b'.') {
            return Err(InvalidName); // SNI hostnames are implicitly absolute.
        }

        linkerd_dns_name::Name::from_str(s).map(|n| Name(Arc::new(n)))
    }
}

impl TryFrom<&[u8]> for Name {
    type Error = InvalidName;

    fn try_from(s: &[u8]) -> Result<Self, Self::Error> {
        if s.last() == Some(&b'.') {
            return Err(InvalidName); // SNI hostnames are implicitly absolute.
        }

        linkerd_dns_name::Name::try_from(s).map(|n| Name(Arc::new(n)))
    }
}

impl AsRef<str> for Name {
    fn as_ref(&self) -> &str {
        (*self.0).as_ref()
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        fmt::Display::fmt(&self.0, f)
    }
}

// === impl LocalId ===

impl From<Name> for LocalId {
    fn from(n: Name) -> Self {
        Self(n)
    }
}

impl From<LocalId> for Name {
    fn from(LocalId(name): LocalId) -> Name {
        name
    }
}

impl LocalId {
    pub fn as_webpki(&self) -> webpki::DNSNameRef<'_> {
        self.0.as_webpki()
    }
}

impl fmt::Display for LocalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
