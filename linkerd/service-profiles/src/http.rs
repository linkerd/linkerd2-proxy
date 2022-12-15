use regex::Regex;
use std::{
    fmt,
    hash::{Hash, Hasher},
    sync::Arc,
};

pub use linkerd_client_policy::http::{
    HttpPolicy, ResponseClass, ResponseClasses, ResponseMatch, Retries, RoutePolicy,
};

#[derive(Clone, Debug)]
pub enum RequestMatch {
    All(Vec<RequestMatch>),
    Any(Vec<RequestMatch>),
    Not(Box<RequestMatch>),
    Path(Box<Regex>),
    Method(http::Method),
}

#[derive(Clone, Default)]
struct Labels(Arc<std::collections::BTreeMap<String, String>>);

pub fn route_for_request<'r, B>(
    http_routes: &'r [(RequestMatch, RoutePolicy)],
    request: &http::Request<B>,
) -> Option<&'r RoutePolicy> {
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
            RequestMatch::Path(ref re) => re.is_match(req.uri().path()),
            RequestMatch::Not(ref m) => !m.is_match(req),
            RequestMatch::All(ref ms) => ms.iter().all(|m| m.is_match(req)),
            RequestMatch::Any(ref ms) => ms.iter().any(|m| m.is_match(req)),
        }
    }
}

// === impl Labels ===

impl PartialEq for Labels {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Labels {}

impl Hash for Labels {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_usize(Arc::as_ref(&self.0) as *const _ as usize);
    }
}

impl fmt::Debug for Labels {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
