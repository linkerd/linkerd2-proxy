use linkerd_dns as dns;
use linkerd_tls::ServerName;

/// Defines a way to match against SNI attributes of the TLS ClientHello
/// message in a TLS handshake. The SNI value being matched is the equivalent
/// of a hostname (as defined in RFC 1123) with 2 notable exceptions:
///
/// 1. IPs are not allowed in SNI names per RFC 6066.
/// 2. A hostname may be prefixed with a wildcard label (`*.`). The wildcard
///    label must appear by itself as the first label.
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
    pub fn summarize_match(&self, sni: &ServerName) -> Option<SniMatch> {
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
