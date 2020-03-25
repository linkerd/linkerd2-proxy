use futures::{Future, Poll};
use http;
use indexmap::IndexMap;
use linkerd2_addr::{Addr, NameAddr};
use linkerd2_error::Error;
use regex::Regex;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::iter::FromIterator;
use std::ops::Deref;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tower::retry::budget::Budget;

mod concrete;
mod requests;
pub mod service;

pub use self::service::Layer;

#[derive(Clone, Debug)]
pub struct WeightedAddr {
    pub addr: NameAddr,
    pub weight: u32,
}

#[derive(Clone, Debug, Default)]
pub struct Routes {
    pub routes: Vec<(RequestMatch, Route)>,
    pub dst_overrides: Vec<WeightedAddr>,
}

pub type Receiver = watch::Receiver<Routes>;
pub type Sender = watch::Sender<Routes>;

/// Watches a destination's Routes.
///
/// The stream updates with all routes for the given destination. The stream
/// never ends and cannot fail.
pub trait GetRoutes<T> {
    type Error: Into<Error>;
    type Future: Future<Item = Receiver, Error = Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error>;

    fn get_routes(&mut self, target: T) -> Self::Future;
}

impl<T, S> GetRoutes<T> for S
where
    T: HasDestination,
    S: tower::Service<Addr, Response = watch::Receiver<Routes>>,
    S::Error: Into<Error>,
{
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        tower::Service::poll_ready(self)
    }

    fn get_routes(&mut self, target: T) -> Self::Future {
        tower::Service::call(self, target.destination())
    }
}

/// Implemented by target types that may be combined with a Route.
pub trait WithRoute {
    type Route;

    fn with_route(self, route: Route) -> Self::Route;
}

/// Implemented by target types that can have their `NameAddr` destination
/// changed.
pub trait OverrideDestination {
    fn dst_mut(&mut self) -> &mut Addr;
}

/// Implemented by target types that may have a `NameAddr` destination that
/// can be discovered via `GetRoutes`.
pub trait HasDestination {
    fn destination(&self) -> Addr;
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Route {
    labels: Labels,
    response_classes: ResponseClasses,
    retries: Option<Retries>,
    timeout: Option<Duration>,
}

#[derive(Clone, Debug)]
pub enum RequestMatch {
    All(Vec<RequestMatch>),
    Any(Vec<RequestMatch>),
    Not(Box<RequestMatch>),
    Path(Regex),
    Method(http::Method),
}

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
struct Labels(Arc<IndexMap<String, String>>);

// === impl Route ===

impl Route {
    pub fn new<I>(label_iter: I, response_classes: Vec<ResponseClass>) -> Self
    where
        I: Iterator<Item = (String, String)>,
    {
        let labels = {
            let mut pairs = label_iter.collect::<Vec<_>>();
            pairs.sort_by(|(k0, _), (k1, _)| k0.cmp(k1));
            Labels(Arc::new(IndexMap::from_iter(pairs)))
        };

        Self {
            labels,
            response_classes: ResponseClasses(response_classes.into()),
            retries: None,
            timeout: None,
        }
    }

    pub fn labels(&self) -> &Arc<IndexMap<String, String>> {
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
            RequestMatch::Path(ref re) => re.is_match(req.uri().path()),
            RequestMatch::Not(ref m) => !m.is_match(req),
            RequestMatch::All(ref ms) => ms.iter().all(|m| m.is_match(req)),
            RequestMatch::Any(ref ms) => ms.iter().any(|m| m.is_match(req)),
        }
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
