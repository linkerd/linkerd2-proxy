use http::Uri;
use regex::Regex;

#[derive(Clone, Debug)]
pub enum MatchPath {
    Exact(String),
    Prefix(String),
    Regex(Regex),
}

/// The number of characters matched in the path.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) enum PathMatch {
    Exact(usize),
    Regex(usize),
    Prefix(usize),
}

// === impl MatchPath ===

impl MatchPath {
    pub(crate) fn match_length(&self, uri: &Uri) -> Option<PathMatch> {
        match self {
            Self::Exact(s) => {
                if s == uri.path() {
                    return Some(PathMatch::Exact(s.len()));
                }
            }

            Self::Regex(re) => {
                if let Some(m) = re.find(uri.path()) {
                    let len = uri.path().len();
                    // Check that the regex is anchored at the start and end of
                    // the value.
                    if m.start() == 0 && m.end() == len {
                        return Some(PathMatch::Regex(len));
                    }
                }
            }

            Self::Prefix(prefix) => {
                if prefix == "/" {
                    return Some(PathMatch::Prefix(1));
                }
                let prefix = prefix.trim_end_matches('/');
                let suffix = uri.path().trim_end_matches('/').strip_prefix(prefix)?;
                // Check that the prefix matches an entire path segment.
                if suffix.is_empty() || suffix.starts_with('/') {
                    return Some(PathMatch::Prefix(prefix.len()));
                }
            }
        }

        None
    }
}

impl std::hash::Hash for MatchPath {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match self {
            Self::Exact(s) => s.hash(state),
            Self::Prefix(s) => s.hash(state),
            Self::Regex(r) => r.as_str().hash(state),
        };
    }
}

impl std::cmp::Eq for MatchPath {}

impl std::cmp::PartialEq for MatchPath {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Exact(s), Self::Exact(o)) => s == o,
            (Self::Prefix(s), Self::Prefix(o)) => s == o,
            (Self::Regex(s), Self::Regex(o)) => s.as_str() == o.as_str(),
            _ => false,
        }
    }
}

// === impl PathMatch ===

impl PathMatch {
    pub fn len(&self) -> usize {
        match self {
            Self::Exact(len) => *len,
            Self::Regex(len) => *len,
            Self::Prefix(len) => *len,
        }
    }
}

impl std::cmp::PartialOrd for PathMatch {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::cmp::Ord for PathMatch {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.len().cmp(&other.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_exact() {
        let m = MatchPath::Exact("/foo/bar".into());
        assert_eq!(
            m.match_length(&"/foo/bar".parse().unwrap()),
            Some(PathMatch::Exact("/foo/bar".len()))
        );
        assert_eq!(m.match_length(&"/foo".parse().unwrap()), None);
        assert_eq!(m.match_length(&"/foo/bah".parse().unwrap()), None);
        assert_eq!(m.match_length(&"/foo/bar/qux".parse().unwrap()), None);
    }

    #[test]
    fn path_prefix() {
        for (pfx, len) in [("/", 1), ("/foo", 4), ("/foo/", 4)] {
            let m = MatchPath::Prefix(pfx.to_string());
            assert_eq!(
                m.match_length(&"/foo".parse().unwrap()),
                Some(PathMatch::Prefix(len)),
                "{pfx} must match /foo",
            );
            assert_eq!(
                m.match_length(&"/foo/".parse().unwrap()),
                Some(PathMatch::Prefix(len)),
                "{pfx} must match /foo/",
            );
            assert_eq!(
                m.match_length(&"/foo/bar".parse().unwrap()),
                Some(PathMatch::Prefix(len)),
                "{pfx} must match /foo/bar",
            );
            assert_eq!(
                m.match_length(&"/foo/bar/qux".parse().unwrap()),
                Some(PathMatch::Prefix(len)),
                "{pfx} must match /foo/bar/qux",
            );
            assert_eq!(
                m.match_length(&"/foobar".parse().unwrap()),
                if len == 1 {
                    Some(PathMatch::Prefix(1))
                } else {
                    None
                }
            );
        }
    }

    #[test]
    fn path_regex() {
        let m = MatchPath::Regex(r#"/foo/\d+"#.parse().unwrap());
        assert_eq!(
            m.match_length(&"/foo/4".parse().unwrap()),
            Some(PathMatch::Regex("/foo/4".len()))
        );
        assert_eq!(
            m.match_length(&"/foo/4321".parse().unwrap()),
            Some(PathMatch::Regex("/foo/4321".len()))
        );
        assert_eq!(m.match_length(&"/bar/foo/4".parse().unwrap()), None);
        assert_eq!(m.match_length(&"/foo/4abc".parse().unwrap()), None);
        assert_eq!(m.match_length(&"/foo/4/bar".parse().unwrap()), None);
        assert_eq!(m.match_length(&"/foo/bar".parse().unwrap()), None);
    }
}
