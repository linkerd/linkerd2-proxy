use std::convert::TryFrom;
use std::fmt;
use thiserror::Error;

/// A `Name` is guaranteed to be syntactically valid. The validity rules
/// are specified in [RFC 5280 Section 7.2], except that underscores are also
/// allowed.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct Name(webpki::DNSName);

#[derive(Copy, Clone, Debug, Eq, PartialEq, Error)]
#[error("invalid DNS name")]
pub struct InvalidName;

impl Name {
    pub fn is_localhost(&self) -> bool {
        use std::str::FromStr;
        *self == Name::from_str("localhost.").unwrap()
    }

    pub fn without_trailing_dot(&self) -> &str {
        self.as_ref().trim_end_matches('.')
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        let s: &str = AsRef::<str>::as_ref(&self.0);
        s.fmt(f)
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        let s: &str = AsRef::<str>::as_ref(&self.0);
        s.fmt(f)
    }
}

impl From<webpki::DNSName> for Name {
    fn from(n: webpki::DNSName) -> Self {
        Name(n)
    }
}

impl<'a> TryFrom<&'a [u8]> for Name {
    type Error = InvalidName;
    fn try_from(s: &[u8]) -> Result<Self, Self::Error> {
        webpki::DNSNameRef::try_from_ascii(s)
            .map_err(|_invalid| InvalidName)
            .map(|r| r.to_owned().into())
    }
}

impl<'a> std::str::FromStr for Name {
    type Err = InvalidName;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::try_from(s.as_bytes())
    }
}

impl Into<webpki::DNSName> for Name {
    fn into(self) -> webpki::DNSName {
        self.0
    }
}

impl<'t> Into<webpki::DNSNameRef<'t>> for &'t Name {
    fn into(self) -> webpki::DNSNameRef<'t> {
        self.0.as_ref()
    }
}

impl AsRef<str> for Name {
    fn as_ref(&self) -> &str {
        <webpki::DNSName as AsRef<str>>::as_ref(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn test_is_localhost() {
        let cases = &[
            ("localhost", false), // Not absolute
            ("localhost.", true),
            ("LocalhOsT.", true),   // Case-insensitive
            ("mlocalhost.", false), // prefixed
            ("localhost1.", false), // suffixed
        ];
        for (host, expected_result) in cases {
            let dns_name = Name::try_from(host.as_bytes()).unwrap();
            assert_eq!(dns_name.is_localhost(), *expected_result, "{:?}", dns_name)
        }
    }

    #[test]
    fn test_without_trailing_dot() {
        let cases = &[
            ("localhost", "localhost"),
            ("localhost.", "localhost"),
            ("LocalhOsT.", "localhost"),
            ("web.svc.local", "web.svc.local"),
            ("web.svc.local.", "web.svc.local"),
        ];
        for (host, expected_result) in cases {
            let dns_name = Name::try_from(host.as_bytes())
                .unwrap_or_else(|_| panic!("'{}' was invalid", host));
            assert_eq!(
                dns_name.without_trailing_dot(),
                *expected_result,
                "{:?}",
                dns_name
            )
        }
        assert!(Name::from_str(".").is_err());
        assert!(Name::from_str("").is_err());
    }
}
