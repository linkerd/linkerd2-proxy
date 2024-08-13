use linkerd_dns as dns;
use linkerd_tls::ServerName;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum MatchSni {
    Exact(String),

    /// Tokenized reverse list of DNS name suffix labels.
    ///
    /// For example: the match `*.example.com` is stored as `["com",
    /// "example"]`.
    Suffix(Vec<String>),
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum SniMatch {
    Exact(usize),
    Suffix(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InvalidSni {
    #[error("invalid sni: {0}")]
    Invalid(#[from] dns::InvalidName),
}

// === impl MatchSni ===

impl std::str::FromStr for MatchSni {
    type Err = InvalidSni;

    fn from_str(sni: &str) -> Result<Self, Self::Err> {
        if let Some(sni) = sni.strip_prefix("*.") {
            return Ok(Self::Suffix(
                sni.split('.').map(|s| s.to_string()).rev().collect(),
            ));
        }

        Ok(Self::Exact(sni.to_string()))
    }
}

impl MatchSni {
    pub fn summarize_match(&self, sni: ServerName) -> Option<SniMatch> {
        let mut sni = sni.as_str();

        match self {
            Self::Exact(h) => {
                if !h.ends_with('.') {
                    sni = sni.strip_suffix('.').unwrap_or(sni);
                }
                if h == sni {
                    Some(SniMatch::Exact(h.len()))
                } else {
                    None
                }
            }

            Self::Suffix(suffix) => {
                if suffix.first().map(|s| &**s) != Some("") {
                    sni = sni.strip_suffix('.').unwrap_or(sni);
                }
                let mut length = 0;
                for sfx in suffix.iter() {
                    sni = sni.strip_suffix(sfx)?;
                    sni = sni.strip_suffix('.')?;
                    length += sfx.len() + 1;
                }

                Some(SniMatch::Suffix(length))
            }
        }
    }
}

// === impl SniMatch ===

impl std::cmp::PartialOrd for SniMatch {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::cmp::Ord for SniMatch {
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
    use linkerd2_proxy_api::tls_route as api;

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidSniMatch {
        #[error("sni match must contain a match")]
        Missing,
    }

    // === impl MatchSni ===

    impl TryFrom<api::SniMatch> for MatchSni {
        type Error = InvalidSniMatch;

        fn try_from(hm: api::SniMatch) -> Result<Self, Self::Error> {
            match hm.r#match.ok_or(InvalidSniMatch::Missing)? {
                api::sni_match::Match::Exact(h) => Ok(MatchSni::Exact(h)),
                api::sni_match::Match::Suffix(sfx) => Ok(MatchSni::Suffix(sfx.reverse_labels)),
            }
        }
    }
}
