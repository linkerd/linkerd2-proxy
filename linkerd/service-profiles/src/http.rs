mod proxy;

use std::{
    fmt,
    hash::{Hash, Hasher},
    ops::Deref,
    sync::Arc,
    time::Duration,
};
use tower::retry::budget::Budget;

pub use self::proxy::NewProxyRouter;
use crate::Targets;

pub type RouteSet = Arc<[(RequestMatch, Route)]>;

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Route {
    labels: Labels,
    response_classes: ResponseClasses,
    retries: Option<Retries>,
    timeout: Option<Duration>,
    // TODO(eliza): I would prefer to rename this to `backends`...
    targets: Targets,
}

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
        min: http::StatusCode,
        max: http::StatusCode,
    },
}

#[derive(Clone, Debug)]
pub struct Retries {
    budget: Arc<Budget>,
}

#[derive(Clone, Default)]
struct Labels(Arc<std::collections::BTreeMap<String, String>>);

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

// === impl Route ===

impl Route {
    pub fn new<I>(label_iter: I, response_classes: Vec<ResponseClass>, targets: Targets) -> Self
    where
        I: Iterator<Item = (String, String)>,
    {
        let labels = Labels(Arc::new(label_iter.collect()));

        Self {
            targets,
            labels,
            response_classes: ResponseClasses(response_classes.into()),
            retries: None,
            timeout: None,
        }
    }

    pub fn labels(&self) -> &Arc<std::collections::BTreeMap<String, String>> {
        &self.labels.0
    }

    pub fn response_classes(&self) -> &ResponseClasses {
        &self.response_classes
    }

    pub fn retries(&self) -> Option<&Retries> {
        self.retries.as_ref()
    }

    pub fn timeout(&self) -> Option<Duration> {
        self.timeout
    }
    pub fn targets(&self) -> &Targets {
        &self.targets
    }

    pub fn set_retries(&mut self, budget: Arc<Budget>) {
        self.retries = Some(Retries { budget });
    }

    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = Some(timeout);
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

// === impl ResponseClass ===

impl ResponseClass {
    pub fn new(is_failure: bool, match_: ResponseMatch) -> Self {
        Self { is_failure, match_ }
    }

    pub fn is_failure(&self) -> bool {
        self.is_failure
    }

    pub fn is_match<B>(&self, req: &http::Response<B>) -> bool {
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

// === impl ResponseMatch ===

impl ResponseMatch {
    fn is_match<B>(&self, req: &http::Response<B>) -> bool {
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
