mod proxy;
pub use self::proxy::NewProxyRouter;
pub use linkerd_client_policy::http::*;
use std::sync::Arc;

pub type RouteSet = Arc<[(RequestMatch, Route)]>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestMatch {
    All(Vec<RequestMatch>),
    Any(Vec<RequestMatch>),
    Not(Box<RequestMatch>),
    Path(Regex),
    Method(http::Method),
    /// The default route always matches all requests.
    Default,
}

/// Wras a `regex::Regex` to implement `PartialEq` via the original string.
#[derive(Clone, Debug)]
pub struct Regex(regex::Regex);

pub fn route_for_request<'r, R, B>(
    http_routes: &'r [(RequestMatch, R)],
    request: &http::Request<B>,
) -> Option<&'r R> {
    for (request_match, route) in http_routes {
        if request_match.is_match(request) {
            return Some(route);
        }
    }
    None
}

// === impl RequestMatch ===

impl RequestMatch {
    fn is_match<B>(&self, req: &http::Request<B>) -> bool {
        match self {
            RequestMatch::Method(ref method) => req.method() == *method,
            RequestMatch::Path(Regex(ref re)) => re.is_match(req.uri().path()),
            RequestMatch::Not(ref m) => !m.is_match(req),
            RequestMatch::All(ref ms) => ms.iter().all(|m| m.is_match(req)),
            RequestMatch::Any(ref ms) => ms.iter().any(|m| m.is_match(req)),
            RequestMatch::Default => true,
        }
    }
}

impl Default for RequestMatch {
    fn default() -> Self {
        Self::Default
    }
}

// === impl Regex ===

impl From<regex::Regex> for Regex {
    fn from(regex: regex::Regex) -> Self {
        Self(regex)
    }
}

impl PartialEq for Regex {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_str() == other.0.as_str()
    }
}

impl Eq for Regex {}
