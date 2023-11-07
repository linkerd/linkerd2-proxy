#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

mod credentials;

pub use self::credentials::{Credentials, DerX509};

/// An endpoint identity descriptor used for authentication.
///
/// Practically speaking, this is a DNS-like identity string that matches an
/// x509 DNS SAN. This will in the future be updated to support SPIFFE IDs as
/// well.
///
/// This isn't restricted to TLS or x509 uses. An authenticated Id could be
/// provided by, e.g., a JWT.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Id(pub linkerd_dns_name::Name);

// === impl Id ===

impl std::str::FromStr for Id {
    type Err = linkerd_error::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // TODO support spiffe:// URIs.
        let n = linkerd_dns_name::Name::from_str(s)?;
        if n.ends_with('.') {
            return Err(linkerd_dns_name::InvalidName.into());
        }
        Ok(Self(n))
    }
}

impl Id {
    pub fn to_str(&self) -> std::borrow::Cow<'_, str> {
        self.0.as_str().into()
    }
}

impl From<linkerd_dns_name::Name> for Id {
    fn from(n: linkerd_dns_name::Name) -> Self {
        Self(n)
    }
}

impl std::fmt::Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.without_trailing_dot().fmt(f)
    }
}
