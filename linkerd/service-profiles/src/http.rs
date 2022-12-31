mod proxy;
pub use self::proxy::NewProxyRouter;
pub use linkerd_client_policy::http::*;
use once_cell::sync::Lazy;
use std::sync::Arc;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteSet(Arc<[(RequestMatch, Route)]>);

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

// === impl RouteSet ===

impl RouteSet {
    #[inline]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn route_for_request<'route, B>(
        &'route self,
        request: &http::Request<B>,
    ) -> Option<&'route Route> {
        for (request_match, route) in self.0.iter() {
            if request_match.is_match(request) {
                return Some(route);
            }
        }
        None
    }
}

impl<B> linkerd_client_policy::MatchRoute<http::Request<B>> for RouteSet {
    type Route = Route;
    fn match_route<'route>(
        &'route self,
        request: &http::Request<B>,
    ) -> Option<&'route Self::Route> {
        self.route_for_request(request)
    }
}

impl<'route> IntoIterator for &'route RouteSet {
    type Item = &'route Route;
    type IntoIter = std::iter::Map<
        std::slice::Iter<'route, (RequestMatch, Route)>,
        fn(&'route (RequestMatch, Route)) -> &'route Route,
    >;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter().map(|(_, r)| r)
    }
}

impl FromIterator<(RequestMatch, Route)> for RouteSet {
    fn from_iter<T: IntoIterator<Item = (RequestMatch, Route)>>(iter: T) -> Self {
        Self(iter.into_iter().collect::<Vec<_>>().into())
    }
}

impl Default for RouteSet {
    fn default() -> Self {
        static EMPTY_ROUTES: Lazy<RouteSet> = Lazy::new(|| std::iter::empty().collect());
        EMPTY_ROUTES.clone()
    }
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
