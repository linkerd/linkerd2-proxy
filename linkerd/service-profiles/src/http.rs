use regex::Regex;
use std::sync::Arc;

pub use linkerd_client_policy::http::{
    HttpPolicy, ResponseClass, ResponseClasses, ResponseMatch, Retries, RoutePolicy,
};
use linkerd_client_policy::{http::FindRoute, Meta};
use once_cell::sync::Lazy;

#[derive(Clone, Debug)]
pub enum RequestMatch {
    All(Vec<RequestMatch>),
    Any(Vec<RequestMatch>),
    Not(Box<RequestMatch>),
    Path(Box<Regex>),
    Method(http::Method),
    /// Avoid compiling lots of `*` regexes for the default route that needs to
    /// match everything.
    Default,
}

#[derive(Clone, Debug)]
pub struct RouteList(Arc<[(RequestMatch, RoutePolicy)]>);

// === impl RouteList ===

impl FindRoute for RouteList {
    type Route = RoutePolicy;

    fn with_routes<F, T>(&self, f: F) -> T
    where
        F: FnOnce(&mut dyn Iterator<Item = Self::Route>) -> T,
    {
        let mut iter = self.0.iter().map(|(_, policy)| policy.clone());
        f(&mut iter)
    }

    fn find_route<'r, B>(&'r self, request: &http::Request<B>) -> Option<&'r Self::Route> {
        for (request_match, route) in self.0.iter() {
            if request_match.is_match(request) {
                tracing::trace!(?route, "Using profile route");
                return Some(route);
            }
        }

        tracing::warn!("No profile route matched");
        None
    }
}

impl Default for RouteList {
    fn default() -> Self {
        Self(vec![default_route()].into())
    }
}

impl FromIterator<(RequestMatch, RoutePolicy)> for RouteList {
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = (RequestMatch, RoutePolicy)>,
    {
        let routes = iter
            .into_iter()
            // add the default route at the end so that the route list does not 404
            .chain(std::iter::once(default_route()))
            .collect::<Vec<_>>();
        Self(routes.into())
    }
}

pub(crate) fn default_route() -> (RequestMatch, RoutePolicy) {
    static DEFAULT_META: Lazy<Arc<Meta>> = Lazy::new(|| {
        Arc::new(Meta::Default {
            name: "default".into(),
        })
    });
    (
        RequestMatch::Default,
        RoutePolicy {
            meta: DEFAULT_META.clone(),
            backends: Vec::new(),
            labels: Default::default(),
            proto: Default::default(),
        },
    )
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
            RequestMatch::Default => true,
        }
    }
}
