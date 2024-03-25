use http::Uri;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum MatchHost {
    Exact(String),

    /// Tokenized reverse list of DNS name suffix labels.
    ///
    /// For example: the match `*.example.com` is stored as `["com",
    /// "example"]`.
    Suffix(Vec<String>),
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum HostMatch {
    Exact(usize),
    Suffix(usize),
}

#[derive(Debug, thiserror::Error)]
pub enum InvalidHost {
    #[error("invalid host: {0}")]
    Invalid(#[from] url::ParseError),

    #[error("host must not be an IP address")]
    NumericHost,
}

// === impl MatchHost ===

impl std::str::FromStr for MatchHost {
    type Err = InvalidHost;

    fn from_str(host: &str) -> Result<Self, Self::Err> {
        use url::Host;
        if let Host::Ipv4(_) | Host::Ipv6(_) = Host::parse(host)? {
            return Err(InvalidHost::NumericHost);
        }

        if let Some(host) = host.strip_prefix("*.") {
            return Ok(Self::Suffix(
                host.split('.').map(|s| s.to_string()).rev().collect(),
            ));
        }

        Ok(Self::Exact(host.to_string()))
    }
}

impl MatchHost {
    pub fn summarize_match(&self, uri: &Uri) -> Option<HostMatch> {
        let mut host = uri.authority()?.host();

        match self {
            Self::Exact(h) => {
                if !h.ends_with('.') {
                    host = host.strip_suffix('.').unwrap_or(host);
                }
                if h == host {
                    Some(HostMatch::Exact(h.len()))
                } else {
                    None
                }
            }

            Self::Suffix(suffix) => {
                if suffix.first().map(|s| &**s) != Some("") {
                    host = host.strip_suffix('.').unwrap_or(host);
                }
                let mut length = 0;
                for sfx in suffix.iter() {
                    host = host.strip_suffix(sfx)?;
                    host = host.strip_suffix('.')?;
                    length += sfx.len() + 1;
                }

                Some(HostMatch::Suffix(length))
            }
        }
    }
}

// === impl HostMatch ===

impl std::cmp::PartialOrd for HostMatch {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::cmp::Ord for HostMatch {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match (self, other) {
            (Self::Exact(l), Self::Exact(r)) => l.cmp(r),
            (Self::Suffix(l), Self::Suffix(r)) => l.cmp(r),
            (Self::Exact(_), Self::Suffix(_)) => Ordering::Greater,
            (Self::Suffix(_), Self::Exact(_)) => Ordering::Less,
        }
    }
}

#[cfg(feature = "proto")]
pub mod proto {
    use super::*;
    use linkerd2_proxy_api::http_route as api;

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidHostMatch {
        #[error("host match must contain a match")]
        Missing,
    }

    // === impl MatchHost ===

    impl TryFrom<api::HostMatch> for MatchHost {
        type Error = InvalidHostMatch;

        fn try_from(hm: api::HostMatch) -> Result<Self, Self::Error> {
            match hm.r#match.ok_or(InvalidHostMatch::Missing)? {
                api::host_match::Match::Exact(h) => Ok(MatchHost::Exact(h)),
                api::host_match::Match::Suffix(sfx) => Ok(MatchHost::Suffix(sfx.reverse_labels)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact() {
        let m = "example.com"
            .parse::<MatchHost>()
            .expect("example.com parses");
        assert_eq!(m, MatchHost::Exact("example.com".to_string()));
        assert_eq!(
            m.summarize_match(&"https://example.com/foo/bar".parse().unwrap()),
            Some(HostMatch::Exact("example.com".len()))
        );
        assert_eq!(
            m.summarize_match(&"https://example.com./foo/bar".parse().unwrap()),
            Some(HostMatch::Exact("example.com".len()))
        );
        assert_eq!(
            m.summarize_match(&"https://foo.example.com/foo/bar".parse().unwrap()),
            None
        );

        let m = "example.com."
            .parse::<MatchHost>()
            .expect("example.com parses");
        assert_eq!(m, MatchHost::Exact("example.com.".to_string()));
        assert_eq!(
            m.summarize_match(&"https://example.com/foo/bar".parse().unwrap()),
            None,
        );
        assert_eq!(
            m.summarize_match(&"https://example.com./foo/bar".parse().unwrap()),
            Some(HostMatch::Exact("example.com.".len()))
        );
    }

    #[test]
    fn suffix() {
        let m = "*.example.com"
            .parse::<MatchHost>()
            .expect("*.example.com parses");
        assert_eq!(
            m,
            MatchHost::Suffix(vec!["com".to_string(), "example".to_string()])
        );
        assert_eq!(
            m.summarize_match(&"https://example.com/foo/bar".parse().unwrap()),
            None
        );
        assert_eq!(
            m.summarize_match(&"https://foo.example.com/foo/bar".parse().unwrap()),
            Some(HostMatch::Suffix(".example.com".len()))
        );
        assert_eq!(
            m.summarize_match(&"https://foo.example.com./foo/bar".parse().unwrap()),
            Some(HostMatch::Suffix(".example.com".len()))
        );
        assert_eq!(
            m.summarize_match(&"https://bar.foo.example.com/foo/bar".parse().unwrap()),
            Some(HostMatch::Suffix(".example.com".len()))
        );

        let m = "*.example.com."
            .parse::<MatchHost>()
            .expect("*.example.com. parses");
        assert_eq!(
            m,
            MatchHost::Suffix(vec![
                "".to_string(),
                "com".to_string(),
                "example".to_string()
            ])
        );
        assert_eq!(
            m.summarize_match(&"https://bar.foo.example.com/foo/bar".parse().unwrap()),
            None
        );
        assert_eq!(
            m.summarize_match(&"https://bar.foo.example.com./foo/bar".parse().unwrap()),
            Some(HostMatch::Suffix(".example.com.".len()))
        );
    }

    #[test]
    fn cmp() {
        assert!(HostMatch::Exact("example.com".len()) > HostMatch::Suffix(".example.com".len()));
        assert!(HostMatch::Exact("foo.example.com".len()) > HostMatch::Exact("example.com".len()));
        assert!(
            HostMatch::Suffix(".foo.example.com".len()) > HostMatch::Suffix(".example.com".len())
        );
        assert_eq!(
            HostMatch::Suffix(".foo.example.com".len()),
            HostMatch::Suffix(".bar.example.com".len())
        );
    }
}
