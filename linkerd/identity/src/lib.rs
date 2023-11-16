#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

mod credentials;

pub use self::credentials::{Credentials, DerX509};
use linkerd_error::Result;
use thiserror::Error;

const SPIFFE_URI_SCHEME: &str = "spiffe://";

/// An endpoint identity descriptor used for authentication.
///
/// Practically speaking, this could be:
/// - a DNS-like identity string that matches an x509 DNS SAN
/// - a SPIFFE ID in URI form, that matches an x509
///
/// This isn't restricted to TLS or x509 uses. An authenticated Id could be
/// provided by, e.g., a JWT.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum Id {
    Dns(linkerd_dns_name::Name),
    Uri(http::Uri),
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Error)]
#[error("invalid TLS id")]
pub struct InvalidId;

// === impl Id ===

impl std::str::FromStr for Id {
    type Err = linkerd_error::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.starts_with(SPIFFE_URI_SCHEME) {
            // TODO: we need to ensure the SPIFFE id is valid
            // according to requirements otlined in:
            // https://github.com/spiffe/spiffe/blob/main/standards/SPIFFE-ID.md#2-spiffe-identity
            return http::Uri::try_from(s).map(Self::Uri).map_err(Into::into);
        }

        // if id does not start with a SPIFFE prefix, assume it is in DNS form
        let n = linkerd_dns_name::Name::from_str(s)?;
        if n.ends_with('.') {
            return Err(InvalidId.into());
        }
        Ok(Self::Dns(n))
    }
}

impl Id {
    pub fn to_str(&self) -> std::borrow::Cow<'_, str> {
        match self {
            Self::Dns(dns) => dns.as_str().into(),
            Self::Uri(uri) => uri.to_string().into(),
        }
    }
}

impl From<linkerd_dns_name::Name> for Id {
    fn from(n: linkerd_dns_name::Name) -> Self {
        Self::Dns(n)
    }
}

impl std::fmt::Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Dns(dns) => dns.without_trailing_dot().fmt(f),
            Self::Uri(uri) => uri.fmt(f),
        }
    }
}
