// Based on https://github.com/briansmith/webpki/blob/18cda8a5e32dfc2723930018853a984bd634e667/src/name/dns_name.rs
//
// Copyright 2015-2020 Brian Smith.
//
// Permission to use, copy, modify, and/or distribute this software for any
// purpose with or without fee is hereby granted, provided that the above
// copyright notice and this permission notice appear in all copies.
//
// THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHORS DISCLAIM ALL WARRANTIES
// WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
// MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHORS BE LIABLE FOR
// ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
// WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
// ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
// OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

use std::{fmt, ops::Deref};
use thiserror::Error;

/// A DNS Name suitable for use in the TLS Server Name Indication (SNI)
/// extension and/or for use as the reference hostname for which to verify a
/// certificate.
///
/// A `Name` is guaranteed to be syntactically valid. The validity rules are
/// specified in [RFC 5280 Section 7.2], except that underscores are also
/// allowed.
///
/// `Name` stores a copy of the input it was constructed from in a `String`
/// and so it is only available when the `std` default feature is enabled.
///
/// [RFC 5280 Section 7.2]: https://tools.ietf.org/html/rfc5280#section-7.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Name(String);

/// A reference to a DNS Name suitable for use in the TLS Server Name Indication
/// (SNI) extension and/or for use as the reference hostname for which to verify
/// a certificate.
///
/// A `NameRef` is guaranteed to be syntactically valid. The validity rules
/// are specified in [RFC 5280 Section 7.2], except that underscores are also
/// allowed.
///
/// [RFC 5280 Section 7.2]: https://tools.ietf.org/html/rfc5280#section-7.2
#[derive(Clone, Copy, Debug, Eq, Hash)]
pub struct NameRef<'a>(&'a str);

#[derive(Copy, Clone, Debug, Eq, PartialEq, Error)]
#[error("invalid DNS name")]
pub struct InvalidName;

// === impl Name ===

impl Name {
    /// Constructs a `Name` if the input is a syntactically-valid DNS name.
    #[inline]
    pub fn try_from_ascii(n: &[u8]) -> Result<Self, InvalidName> {
        let n = NameRef::try_from_ascii(n)?;
        Ok(n.to_owned())
    }

    #[inline]
    pub fn as_ref(&self) -> NameRef<'_> {
        NameRef(self.0.as_str())
    }

    #[inline]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    #[inline]
    pub fn is_localhost(&self) -> bool {
        self.as_str().eq_ignore_ascii_case("localhost.")
    }

    #[inline]
    pub fn without_trailing_dot(&self) -> &str {
        self.as_str().trim_end_matches('.')
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for Name {
    type Err = InvalidName;

    #[inline]
    fn from_str(n: &str) -> Result<Self, Self::Err> {
        Self::try_from_ascii(n.as_bytes())
    }
}

impl Deref for Name {
    type Target = str;

    #[inline]
    fn deref(&self) -> &str {
        self.0.as_str()
    }
}

// === impl NameRef ===

impl<'a> NameRef<'a> {
    /// Constructs a `NameRef` from the given input if the input is a
    /// syntactically-valid DNS name.
    pub fn try_from_ascii(dns_name: &'a [u8]) -> Result<Self, InvalidName> {
        if !is_valid_reference_dns_id(untrusted::Input::from(dns_name)) {
            return Err(InvalidName);
        }

        let s = std::str::from_utf8(dns_name).map_err(|_| InvalidName)?;
        Ok(Self(s))
    }

    pub fn try_from_ascii_str(n: &'a str) -> Result<Self, InvalidName> {
        Self::try_from_ascii(n.as_bytes())
    }

    /// Constructs a `Name` from this `NameRef`
    pub fn to_owned(self) -> Name {
        // NameRef is already guaranteed to be valid ASCII, which is a
        // subset of UTF-8.
        Name(self.as_str().to_ascii_lowercase())
    }

    #[inline]
    pub fn as_str(&self) -> &str {
        self.0
    }

    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl<'a> PartialEq<NameRef<'a>> for NameRef<'_> {
    fn eq(&self, other: &NameRef<'a>) -> bool {
        self.0.eq_ignore_ascii_case(other.0)
    }
}

impl fmt::Display for NameRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        self.as_str().fmt(f)
    }
}

// === Helpers ===

fn is_valid_reference_dns_id(hostname: untrusted::Input<'_>) -> bool {
    is_valid_dns_id(hostname)
}

// https://tools.ietf.org/html/rfc5280#section-4.2.1.6:
//
//   When the subjectAltName extension contains a domain name system
//   label, the domain name MUST be stored in the DnsName (an IA5String).
//   The name MUST be in the "preferred name syntax", as specified by
//   Section 3.5 of [RFC1034] and as modified by Section 2.1 of
//   [RFC1123].
//
// https://bugzilla.mozilla.org/show_bug.cgi?id=1136616: As an exception to the
// requirement above, underscores are also allowed in names for compatibility.
fn is_valid_dns_id(hostname: untrusted::Input<'_>) -> bool {
    // https://blogs.msdn.microsoft.com/oldnewthing/20120412-00/?p=7873/
    if hostname.len() > 253 {
        return false;
    }

    let mut input = untrusted::Reader::new(hostname);

    let mut label_length = 0;
    let mut label_is_all_numeric = false;
    let mut label_ends_with_hyphen = false;

    loop {
        const MAX_LABEL_LENGTH: usize = 63;

        match input.read_byte() {
            Ok(b'-') => {
                if label_length == 0 {
                    return false; // Labels must not start with a hyphen.
                }
                label_is_all_numeric = false;
                label_ends_with_hyphen = true;
                label_length += 1;
                if label_length > MAX_LABEL_LENGTH {
                    return false;
                }
            }

            Ok(b'0'..=b'9') => {
                if label_length == 0 {
                    label_is_all_numeric = true;
                }
                label_ends_with_hyphen = false;
                label_length += 1;
                if label_length > MAX_LABEL_LENGTH {
                    return false;
                }
            }

            Ok(b'a'..=b'z') | Ok(b'A'..=b'Z') | Ok(b'_') => {
                label_is_all_numeric = false;
                label_ends_with_hyphen = false;
                label_length += 1;
                if label_length > MAX_LABEL_LENGTH {
                    return false;
                }
            }

            Ok(b'.') => {
                if label_ends_with_hyphen {
                    return false; // Labels must not end with a hyphen.
                }
                if label_length == 0 {
                    return false;
                }
                label_length = 0;
            }

            _ => {
                return false;
            }
        }

        if input.at_end() {
            break;
        }
    }

    if label_ends_with_hyphen {
        return false; // Labels must not end with a hyphen.
    }

    if label_is_all_numeric {
        return false; // Last label must not be all numeric.
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse() {
        const CASES: &[(&str, bool)] = &[
            ("", false),
            (".", false),
            ("..", false),
            ("...", false),
            ("*", false),
            ("a", true),
            ("a.", true),
            ("d.c.b.a", true),
            ("d.c.b.a.", true),
            (" d.c.b.a.", false),
            ("d.c.b.a-", false),
            ("*.a.", false),
            (".a.", false),
            ("a1", true),
            ("_m.foo.bar", true),
            ("m.foo.bar_", true),
            ("example.com:80", false),
            ("1", false),
            ("1.a", true),
            ("a.1", false),
            ("1.2.3.4", false),
            ("::1", false),
            ("xn--poema-9qae5a.com.br", true), // IDN
        ];
        for &(n, expected_result) in CASES {
            assert!(n.parse::<Name>().is_ok() == expected_result, "{}", n);
        }
    }

    #[test]
    fn test_eq() {
        const CASES: &[(&str, &str, bool)] = &[
            ("a", "a", true),
            ("a", "b", false),
            ("d.c.b.a", "d.c.b.a", true),
            // case sensitivity
            (
                "abcdefghijklmnopqrstuvwxyz",
                "ABCDEFGHIJKLMNOPQRSTUVWXYZ",
                true,
            ),
            ("aBc", "Abc", true),
            ("a1", "A1", true),
            ("example", "example", true),
            ("example.", "example.", true),
            ("example", "example.", false),
            ("example.com", "example.com", true),
            ("example.com.", "example.com.", true),
            ("example.com", "example.com.", false),
        ];
        for &(left, right, expected_result) in CASES {
            let l = left.parse::<Name>().unwrap();
            let r = right.parse::<Name>().unwrap();
            assert_eq!(l == r, expected_result, "{:?} vs {:?}", l, r);
        }
    }

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
            let dns_name = host.parse::<Name>().unwrap();
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
            let dns_name = host
                .parse::<Name>()
                .unwrap_or_else(|_| panic!("'{}' was invalid", host));
            assert_eq!(
                dns_name.without_trailing_dot(),
                *expected_result,
                "{:?}",
                dns_name
            )
        }
        assert!(".".parse::<Name>().is_err());
        assert!("..".parse::<Name>().is_err());
        assert!("".parse::<Name>().is_err());
    }
}
