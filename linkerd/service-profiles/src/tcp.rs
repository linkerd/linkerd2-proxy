use crate::Backends;
use once_cell::sync::Lazy;
use std::sync::Arc;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RequestMatch(());

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteSet(Arc<[(RequestMatch, Route)]>);

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Route {
    backends: Backends,
}

impl Route {
    #[must_use]
    pub fn new(backends: Backends) -> Self {
        Self { backends }
    }

    pub fn backends(&self) -> &Backends {
        &self.backends
    }
}

// === impl RequestMatch ===

impl RequestMatch {
    fn is_match<T>(&self, _request: &T) -> bool {
        // TODO(eliza): eventually we'll actually have to match TCP routes.
        true
    }
}

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
}

impl<T> linkerd_client_policy::MatchRoute<T> for RouteSet {
    type Route = Route;

    fn match_route<'route>(&'route self, request: &T) -> Option<&'route Self::Route> {
        for (req_match, route) in self.0.iter() {
            if req_match.is_match(request) {
                return Some(route);
            }
        }
        None
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
        static DEFAULT_ROUTES: Lazy<RouteSet> =
            Lazy::new(|| std::iter::once(Default::default()).collect());
        DEFAULT_ROUTES.clone()
    }
}
