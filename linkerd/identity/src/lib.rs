#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

mod credentials;

pub use self::credentials::{Credentials, DerX509};
use linkerd_error::{Error, Result};
use std::convert::From;
use std::str::FromStr;

/// An endpoint identity descriptor used for authentication.
///
/// Practically speaking, this could be:
/// - a DNS-like identity string that matches an x509 DNS SAN
/// - an identity in URI form, that matches an x509 URI SAN
///
/// This isn't restricted to TLS or x509 uses. An authenticated Id could be
/// provided by, e.g., a JWT.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum Id {
    Dns(linkerd_dns_name::Name),
    Uri(url::Url),
}

#[derive(Debug, thiserror::Error)]
#[error("invalid TLS id: {0}")]
pub struct InvalidId(#[source] Error);

// === impl Id ===

impl std::str::FromStr for Id {
    type Err = linkerd_error::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse_dns_name(s)
            .or_else(|_| Self::parse_uri(s))
            .map_err(Into::into)
    }
}

impl Id {
    pub fn parse_dns_name(s: &str) -> Result<Self, InvalidId> {
        linkerd_dns_name::Name::from_str(s)
            .map_err(|e| InvalidId(e.into()))
            .and_then(|n| {
                if n.ends_with('.') {
                    Err(InvalidId(linkerd_dns_name::InvalidName.into()))
                } else {
                    Ok(Self::Dns(n))
                }
            })
    }

    pub fn parse_uri(s: &str) -> Result<Self, InvalidId> {
        url::Url::parse(s)
            .map(Self::Uri)
            .map_err(|e| InvalidId(e.into()))
    }

    pub fn to_str(&self) -> std::borrow::Cow<'_, str> {
        match self {
            Self::Dns(dns) => dns.as_str().into(),
            Self::Uri(uri) => uri.as_str().into(),
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

#[cfg(test)]
mod tests {
    use super::Id;

    #[test]
    fn roundtrip_uri_id_parsing_spiffe() {
        let id: Id = "spiffe://host:1234/path".parse().unwrap();
        assert_eq!(id, id.to_string().parse().unwrap());
    }

    #[test]
    fn roundtrip_uri_id_parsing_non_spiffe() {
        let id: Id = "http://host:1234/path".parse().unwrap();
        assert_eq!(id, id.to_string().parse().unwrap());
    }

    #[test]
    fn roundtrip_uri_dns_parsing() {
        let id: Id = "some-svc.svc.cluster.local".parse().unwrap();
        assert_eq!(id, id.to_string().parse().unwrap());
    }

    #[test]
    fn cannot_parse_uri_as_dns() {
        assert!(Id::parse_dns_name("uri://host:1234/path").is_err());
    }

    #[test]
    fn cannot_parse_dns_name_as_uri() {
        assert!(Id::parse_uri("some-svc.svc.cluster.local").is_err());
    }
}
