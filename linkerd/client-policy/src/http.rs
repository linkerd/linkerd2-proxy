use linkerd_http_route::http;
pub use linkerd_http_route::http::*;
use once_cell::sync::Lazy;
use std::{
    fmt,
    hash::{Hash, Hasher},
    ops::Deref,
    sync::Arc,
    time::Duration,
};
use tower::retry::budget::Budget;

mod router;
pub use self::router::*;

pub type RoutePolicy = crate::RoutePolicy<HttpPolicy>;
pub type Route = http::Route<RoutePolicy>;
pub type Rule = http::Rule<RoutePolicy>;

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct HttpPolicy {
    pub response_classes: ResponseClasses,
    pub retries: Option<Retries>,
    pub timeout: Option<Duration>,
}

#[inline]
pub fn find<'r, B>(
    routes: &'r [Route],
    req: &::http::Request<B>,
) -> Option<(http::RouteMatch, &'r RoutePolicy)> {
    http::find(routes, req)
}

pub(super) static NO_ROUTES: Lazy<Arc<[Route]>> = Lazy::new(|| vec![].into());

#[derive(Clone, Debug)]
pub struct ResponseClass {
    is_failure: bool,
    match_: ResponseMatch,
}

#[derive(Clone, Default)]
pub struct ResponseClasses(Arc<Vec<ResponseClass>>);

#[derive(Clone, Debug)]
pub enum ResponseMatch {
    All(Vec<ResponseMatch>),
    Any(Vec<ResponseMatch>),
    Not(Box<ResponseMatch>),
    Status {
        min: ::http::StatusCode,
        max: ::http::StatusCode,
    },
}

#[derive(Clone, Debug)]
pub struct Retries {
    budget: Arc<Budget>,
}

// === impl RoutePolicy ===

impl HttpPolicy {
    pub fn new(response_classes: Vec<ResponseClass>) -> Self {
        Self {
            response_classes: ResponseClasses(response_classes.into()),
            retries: None,
            timeout: None,
        }
    }
}

impl RoutePolicy {
    pub fn response_classes(&self) -> &ResponseClasses {
        &self.proto.response_classes
    }

    pub fn retries(&self) -> Option<&Retries> {
        self.proto.retries.as_ref()
    }

    pub fn timeout(&self) -> Option<Duration> {
        self.proto.timeout
    }

    pub fn set_retries(&mut self, budget: Arc<Budget>) {
        self.proto.retries = Some(Retries { budget });
    }

    pub fn set_timeout(&mut self, timeout: Duration) {
        self.proto.timeout = Some(timeout);
    }
}

// === impl ResponseClass ===

impl ResponseClass {
    pub fn new(is_failure: bool, match_: ResponseMatch) -> Self {
        Self { is_failure, match_ }
    }

    pub fn is_failure(&self) -> bool {
        self.is_failure
    }

    pub fn is_match<B>(&self, req: &::http::Response<B>) -> bool {
        self.match_.is_match(req)
    }
}

// === impl ResponseClasses ===

impl Deref for ResponseClasses {
    type Target = [ResponseClass];

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl PartialEq for ResponseClasses {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for ResponseClasses {}

impl Hash for ResponseClasses {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_usize(Arc::as_ref(&self.0) as *const _ as usize);
    }
}

impl fmt::Debug for ResponseClasses {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromIterator<ResponseClass> for ResponseClasses {
    fn from_iter<T: IntoIterator<Item = ResponseClass>>(iter: T) -> Self {
        Self(iter.into_iter().collect::<Vec<_>>().into())
    }
}

// === impl ResponseMatch ===

impl ResponseMatch {
    fn is_match<B>(&self, req: &::http::Response<B>) -> bool {
        match self {
            ResponseMatch::Status { ref min, ref max } => {
                *min <= req.status() && req.status() <= *max
            }
            ResponseMatch::Not(ref m) => !m.is_match(req),
            ResponseMatch::All(ref ms) => ms.iter().all(|m| m.is_match(req)),
            ResponseMatch::Any(ref ms) => ms.iter().any(|m| m.is_match(req)),
        }
    }
}

// === impl Retries ===

impl Retries {
    pub fn budget(&self) -> &Arc<Budget> {
        &self.budget
    }
}

impl PartialEq for Retries {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.budget, &other.budget)
    }
}

impl Eq for Retries {}

impl Hash for Retries {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_usize(Arc::as_ref(&self.budget) as *const _ as usize);
    }
}

#[cfg(feature = "proto")]
pub mod proto {
    use super::*;
    use crate::{meta::proto::InvalidMeta, proto::InvalidBackend, split::Backend, Meta};
    use linkerd2_proxy_api::outbound as api;
    use linkerd_http_route::http::r#match::{
        host::proto::InvalidHostMatch, proto::InvalidRouteMatch,
    };
    use std::sync::Arc;

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidHttpRoute {
        #[error("invalid host match: {0}")]
        HostMatch(#[from] InvalidHostMatch),

        #[error("invalid route match: {0}")]
        RouteMatch(#[from] InvalidRouteMatch),
        #[error("invalid labels: {0}")]
        Meta(#[from] InvalidMeta),
        #[error("invalid backend: {0}")]
        Backend(#[from] InvalidBackend),
    }

    pub fn try_route(proto: api::HttpRoute) -> Result<Route, InvalidHttpRoute> {
        let api::HttpRoute {
            hosts,
            rules,
            metadata,
        } = proto;

        let hosts = hosts
            .into_iter()
            .map(r#match::MatchHost::try_from)
            .collect::<Result<Vec<_>, InvalidHostMatch>>()?;

        let meta = Arc::new(Meta::try_from(metadata.ok_or(InvalidMeta::Missing)?)?);
        let rules = rules
            .into_iter()
            .map(|r| try_rule(meta.clone(), r))
            .collect::<Result<Vec<_>, InvalidHttpRoute>>()?;

        Ok(Route { hosts, rules })
    }

    fn try_rule(meta: Arc<Meta>, proto: api::http_route::Rule) -> Result<Rule, InvalidHttpRoute> {
        let api::http_route::Rule { matches, backends } = proto;
        let matches = matches
            .into_iter()
            .map(r#match::MatchRequest::try_from)
            .collect::<Result<Vec<_>, InvalidRouteMatch>>()?;
        let backends = backends
            .into_iter()
            .map(Backend::try_from)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Rule {
            matches,
            policy: RoutePolicy {
                backends,
                meta,
                // the outbound policy API doesn't currently have fields for
                // these forms of client policy.
                proto: HttpPolicy::default(),
            },
        })
    }
}
