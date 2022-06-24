use http::uri::Uri;
use regex::Regex;

#[derive(Clone, Debug)]
pub enum MatchQueryParam {
    Exact(String, String),
    Regex(String, Regex),
}

// === impl MatchQueryParam ===

impl MatchQueryParam {
    pub fn is_match(&self, uri: &Uri) -> bool {
        uri.query().map_or(false, |qs| {
            url::form_urlencoded::parse(qs.as_bytes()).any(|(q, p)| match self {
                Self::Exact(n, v) => *n == *q && *v == *p,
                Self::Regex(n, r) => {
                    if *n == *q {
                        if let Some(m) = r.find(&*p) {
                            // Check that the regex is anchored at the start and
                            // end of the value.
                            return m.start() == 0 && m.end() == p.len();
                        }
                    }
                    false
                }
            })
        })
    }
}

impl std::hash::Hash for MatchQueryParam {
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

impl std::cmp::Eq for MatchQueryParam {}

impl std::cmp::PartialEq for MatchQueryParam {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Exact(n, s), Self::Exact(m, o)) => n == m && s == o,
            (Self::Regex(n, s), Self::Regex(m, o)) => n == m && s.as_str() == o.as_str(),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::MatchQueryParam;

    #[test]
    fn query_param_exact() {
        let m = MatchQueryParam::Exact("foo".to_string(), "bar".to_string());
        assert!(m.is_match(&"/?foo=bar".parse().unwrap()));
        assert!(m.is_match(&"/?foo=bar&fah=bah".parse().unwrap()));
        assert!(m.is_match(&"/?fah=bah&foo=bar".parse().unwrap()));
        assert!(!m.is_match(&"/?foo=bah".parse().unwrap()));

        let m = MatchQueryParam::Exact("foo".to_string(), "".to_string());
        assert!(m.is_match(&"/?foo".parse().unwrap()));
        assert!(!m.is_match(&"/?foo=bah".parse().unwrap()));
        assert!(!m.is_match(&"/?bar=foo".parse().unwrap()));
    }

    #[test]
    fn query_param_regex() {
        let m = MatchQueryParam::Regex("foo".to_string(), "bar*".parse().unwrap());
        assert!(m.is_match(&"/?foo=bar".parse().unwrap()));
        assert!(m.is_match(&"/?foo=ba&fah=bah".parse().unwrap()));
        assert!(m.is_match(&"/?foo=barr&fah=bah".parse().unwrap()));
        assert!(m.is_match(&"/?bar=foo&foo=bar".parse().unwrap()));
        assert!(m.is_match(&"/?foo=bah&foo=bar".parse().unwrap()));
        assert!(!m.is_match(&"/?foo=bah".parse().unwrap()));
        assert!(!m.is_match(&"/?bar=foo".parse().unwrap()));
        assert!(!m.is_match(&"/?foo=barro".parse().unwrap()));

        let m = MatchQueryParam::Regex("foo".to_string(), "bar/.+".parse().unwrap());
        assert!(m.is_match(&"/?foo=bar%2Fhi".parse().unwrap()));
        assert!(!m.is_match(&"/?foo=bah%20hi".parse().unwrap()));

        let m = MatchQueryParam::Regex("foo".to_string(), ".*".parse().unwrap());
        assert!(m.is_match(&"/?foo".parse().unwrap()));
    }
}
