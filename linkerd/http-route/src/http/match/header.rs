use http::header::{HeaderName, HeaderValue};
use regex::Regex;

/// Matches a single HTTP header value.
#[derive(Clone, Debug)]
pub enum MatchHeader {
    Exact(HeaderName, HeaderValue),
    Regex(HeaderName, Regex),
}

// === impl MatchHeader ===

impl MatchHeader {
    pub fn is_match(&self, headers: &http::HeaderMap) -> bool {
        match self {
            Self::Exact(n, v) => headers.get_all(n).iter().any(|h| h == v),
            Self::Regex(n, re) => headers
                .get_all(n)
                .iter()
                .filter_map(|h| h.to_str().ok())
                .any(|h| {
                    if let Some(m) = re.find(h) {
                        // Check that the regex is anchored at the start and end
                        // of the value.
                        m.start() == 0 && m.end() == h.len()
                    } else {
                        false
                    }
                }),
        }
    }
}

impl std::hash::Hash for MatchHeader {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match self {
            Self::Exact(n, s) => {
                n.hash(state);
                s.hash(state)
            }
            Self::Regex(n, r) => {
                n.hash(state);
                r.as_str().hash(state);
            }
        }
    }
}

impl std::cmp::Eq for MatchHeader {}

impl std::cmp::PartialEq for MatchHeader {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Exact(n, s), Self::Exact(m, o)) => n == m && s == o,
            (Self::Regex(n, s), Self::Regex(m, o)) => n == m && s.as_str() == o.as_str(),
            _ => false,
        }
    }
}

#[cfg(feature = "proto")]
pub mod proto {
    use super::*;
    use linkerd2_proxy_api::http_route as api;

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidHeaderMatch {
        #[error("{0}")]
        InvalidName(#[from] http::header::InvalidHeaderName),

        #[error("missing a header value match")]
        MissingValueMatch,

        #[error("{0}")]
        InvalidValue(#[from] http::header::InvalidHeaderValue),

        #[error("invalid regular expression: {0}")]
        InvalidRegex(#[from] regex::Error),
    }

    // === impl MatchHeader ===

    impl TryFrom<api::HeaderMatch> for MatchHeader {
        type Error = InvalidHeaderMatch;

        fn try_from(hm: api::HeaderMatch) -> Result<Self, Self::Error> {
            let name = http::header::HeaderName::from_bytes(hm.name.as_bytes())?;
            match hm.value.ok_or(InvalidHeaderMatch::MissingValueMatch)? {
                api::header_match::Value::Exact(h) => Ok(MatchHeader::Exact(name, h.try_into()?)),
                api::header_match::Value::Regex(re) => Ok(MatchHeader::Regex(name, re.parse()?)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_exact() {
        let m = MatchHeader::Exact(
            HeaderName::from_static("foo"),
            HeaderValue::from_static("bar"),
        );
        assert!(m.is_match(&{
            let mut h = http::HeaderMap::new();
            h.insert(
                HeaderName::from_static("foo"),
                HeaderValue::from_static("bar"),
            );
            h
        }));
        assert!(m.is_match(&{
            let mut h = http::HeaderMap::new();
            h.append(
                HeaderName::from_static("foo"),
                HeaderValue::from_static("bah"),
            );
            h.append(
                HeaderName::from_static("foo"),
                HeaderValue::from_static("bar"),
            );
            h.append(
                HeaderName::from_static("foo"),
                HeaderValue::from_static("baz"),
            );
            h
        }));
        assert!(!m.is_match(&{
            let mut h = http::HeaderMap::new();
            h.append(
                HeaderName::from_static("foo"),
                HeaderValue::from_static("bah"),
            );
            h
        }));
    }

    #[test]
    fn headers_regex() {
        let m = MatchHeader::Regex(HeaderName::from_static("foo"), "bar*".parse().unwrap());
        assert!(m.is_match(&{
            let mut h = http::HeaderMap::new();
            h.insert(
                HeaderName::from_static("foo"),
                HeaderValue::from_static("bar"),
            );
            h
        }));
        assert!(m.is_match(&{
            let mut h = http::HeaderMap::new();
            h.append(
                HeaderName::from_static("foo"),
                HeaderValue::from_static("bahh"),
            );
            h.append(
                HeaderName::from_static("foo"),
                HeaderValue::from_static("barr"),
            );
            h.append(
                HeaderName::from_static("foo"),
                HeaderValue::from_static("bazz"),
            );
            h
        }));
        assert!(!m.is_match(&{
            let mut h = http::HeaderMap::new();
            h.append(
                HeaderName::from_static("foo"),
                HeaderValue::from_static("bah"),
            );
            h
        }));
        assert!(!m.is_match(&{
            let mut h = http::HeaderMap::new();
            h.append(
                HeaderName::from_static("foo"),
                HeaderValue::from_static("barro"),
            );
            h
        }));
    }
}
