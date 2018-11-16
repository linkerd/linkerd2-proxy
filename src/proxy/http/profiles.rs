#![allow(dead_code)]

extern crate tower_discover;

use futures::Stream;
use http;
use indexmap::IndexMap;
use regex::Regex;
use std::iter::FromIterator;
use std::sync::Arc;
use std::{error, fmt};

use NameAddr;

pub type Routes = Vec<(RequestMatch, Route)>;

/// Watches a destination's Routes.
///
/// The stream updates with all routes for the given destination. The stream
/// never ends and cannot fail.
pub trait GetRoutes {
    type Stream: Stream<Item = Routes, Error = Error>;

    fn get_routes(&self, dst: &NameAddr) -> Option<Self::Stream>;
}

/// Implemented by target types that may be combined with a Route.
pub trait WithRoute {
    type Output;

    fn with_route(self, route: Route) -> Self::Output;
}

/// Implemented by target types that may have a `NameAddr` destination that
/// can be discovered via `GetRoutes`.
pub trait CanGetDestination {
    fn get_destination(&self) -> Option<&NameAddr>;
}

#[derive(Debug)]
pub enum Error {}

#[derive(Clone, Debug, Default)]
pub struct Route {
    labels: Arc<IndexMap<String, String>>,
    response_classes: ResponseClasses,
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

pub type ResponseClasses = Arc<Vec<ResponseClass>>;

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

// === impl Route ===

impl Route {
    pub fn new<I>(label_iter: I, response_classes: Vec<ResponseClass>) -> Self
    where
        I: Iterator<Item = (String, String)>,
    {
        let labels = {
            let mut pairs = label_iter.collect::<Vec<_>>();
            pairs.sort_by(|(k0, _), (k1, _)| k0.cmp(k1));
            Arc::new(IndexMap::from_iter(pairs))
        };

        Self {
            labels,
            response_classes: response_classes.into(),
        }
    }

    pub fn labels(&self) -> &Arc<IndexMap<String, String>> {
        &self.labels
    }

    pub fn response_classes(&self) -> &ResponseClasses {
        &self.response_classes
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

// === impl Error ===

impl fmt::Display for Error {
    fn fmt(&self, _: &mut fmt::Formatter) -> fmt::Result {
        unreachable!()
    }
}

impl error::Error for Error {}

/// A stack module that produces a Service that routes requests through alternate
/// middleware configurations
///
/// As the router's Stack is built, a destination is extracted from the stack's
/// target and it is used to get route profiles from ` GetRoutes` implemetnation.
///
/// Each route uses a shared underlying stack. As such, it assumed that the
/// underlying stack is buffered, and so `poll_ready` is NOT called on the routes
/// before requests are dispatched. If an individual route wishes to apply
/// backpressure, it must implement its own buffer/limit strategy.
pub mod router {
    use futures::{Async, Poll, Stream};
    use http;
    use std::{error, fmt};

    use dns;
    use svc;

    use super::*;

    pub fn layer<T, G, M, R>(suffixes: Vec<dns::Suffix>, get_routes: G, route_layer: R)
        -> Layer<G, M, R>
    where
        T: CanGetDestination + WithRoute + Clone,
        M: svc::Stack<T>,
        M::Value: Clone,
        G: GetRoutes + Clone,
        R: svc::Layer<
                <T as WithRoute>::Output,
                <T as WithRoute>::Output,
                svc::shared::Stack<M::Value>,
            > + Clone,
    {
        Layer {
            suffixes,
            get_routes,
            route_layer,
            default_route: Route::default(),
            _p: ::std::marker::PhantomData,
        }
    }

    #[derive(Clone, Debug)]
    pub struct Layer<G, M, R = ()> {
        get_routes: G,
        route_layer: R,
        default_route: Route,
        suffixes: Vec<dns::Suffix>,
        _p: ::std::marker::PhantomData<fn() -> M>,
    }

    #[derive(Clone, Debug)]
    pub struct Stack<G, M, R = ()> {
        inner: M,
        get_routes: G,
        route_layer: R,
        default_route: Route,
        suffixes: Vec<dns::Suffix>,
    }

    #[derive(Debug)]
    pub enum Error<D, R> {
        Inner(D),
        Route(R),
    }

    pub struct Service<G, T, R>
    where
        T: WithRoute,
        R: svc::Stack<T::Output>,
    {
        target: T,
        stack: R,
        route_stream: Option<G>,
        routes: Vec<(RequestMatch, R::Value)>,
        default_route: R::Value,
    }

    impl<D: fmt::Display, R: fmt::Display> fmt::Display for Error<D, R> {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            match self {
                Error::Inner(e) => fmt::Display::fmt(&e, f),
                Error::Route(e) => fmt::Display::fmt(&e, f),
            }
        }
    }

    impl<D: error::Error, R: error::Error> error::Error for Error<D, R> {}

    impl<T, G, M, R> svc::Layer<T, T, M> for Layer<G, M, R>
    where
        T: CanGetDestination + WithRoute + Clone,
        G: GetRoutes + Clone,
        M: svc::Stack<T>,
        M::Value: Clone,
        R: svc::Layer<
                <T as WithRoute>::Output,
                <T as WithRoute>::Output,
                svc::shared::Stack<M::Value>,
            > + Clone,
    {
        type Value = <Stack<G, M, R> as svc::Stack<T>>::Value;
        type Error = <Stack<G, M, R> as svc::Stack<T>>::Error;
        type Stack = Stack<G, M, R>;

        fn bind(&self, inner: M) -> Self::Stack {
            Stack {
                inner,
                get_routes: self.get_routes.clone(),
                route_layer: self.route_layer.clone(),
                default_route: self.default_route.clone(),
                suffixes: self.suffixes.clone(),
            }
        }
    }

    impl<T, G, M, R> svc::Stack<T> for Stack<G, M, R>
    where
        T: CanGetDestination + WithRoute + Clone,
        M: svc::Stack<T>,
        M::Value: Clone,
        G: GetRoutes,
        R: svc::Layer<
                <T as WithRoute>::Output,
                <T as WithRoute>::Output,
                svc::shared::Stack<M::Value>,
            > + Clone,
    {
        type Value = Service<G::Stream, T, R::Stack>;
        type Error = Error<M::Error, R::Error>;

        fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
            let inner = self.inner.make(&target).map_err(Error::Inner)?;
            let stack = self.route_layer.bind(svc::shared::stack(inner));

            let default_route = {
                let t = target.clone().with_route(self.default_route.clone());
                stack.make(&t).map_err(Error::Route)?
            };

            let route_stream = match target.get_destination() {
                Some(ref dst) => {
                    if self.suffixes.iter().any(|s| s.contains(dst.name())) {
                        debug!("fetching routes for {:?}", dst);
                        self.get_routes.get_routes(&dst)
                    } else {
                        debug!("skipping route discovery for dst={:?}", dst);
                        None
                    }
                }
                None => {
                    debug!("no destination for routes");
                    None
                }
            };

            Ok(Service {
                target: target.clone(),
                stack,
                route_stream,
                default_route,
                routes: Vec::new(),
            })
        }
    }

    impl<G, T, R> Service<G, T, R>
    where
        G: Stream<Item = Routes, Error = super::Error>,
        T: WithRoute + Clone,
        R: svc::Stack<T::Output> + Clone,
    {
        fn update_routes(&mut self, mut routes: Routes) {
            self.routes = Vec::with_capacity(routes.len());
            for (req_match, route) in routes.drain(..) {
                let target = self.target.clone().with_route(route.clone());
                match self.stack.make(&target) {
                    Ok(svc) => self.routes.push((req_match, svc)),
                    Err(_) => error!("failed to build service for route: route={:?}", route),
                }
            }
        }

        fn poll_route_stream(&mut self) -> Option<Async<Option<Routes>>> {
            self.route_stream
                .as_mut()
                .and_then(|ref mut s| s.poll().ok())
        }
    }

    impl<G, T, R, B> svc::Service<http::Request<B>> for Service<G, T, R>
    where
        G: Stream<Item = Routes, Error = super::Error>,
        T: WithRoute + Clone,
        R: svc::Stack<T::Output> + Clone,
        R::Value: svc::Service<http::Request<B>>,
    {
        type Response = <R::Value as svc::Service<http::Request<B>>>::Response;
        type Error = <R::Value as svc::Service<http::Request<B>>>::Error;
        type Future = <R::Value as svc::Service<http::Request<B>>>::Future;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            while let Some(Async::Ready(Some(routes))) = self.poll_route_stream() {
                self.update_routes(routes);
            }

            Ok(Async::Ready(()))
        }

        fn call(&mut self, req: http::Request<B>) -> Self::Future {
            for (ref condition, ref mut service) in &mut self.routes {
                if condition.is_match(&req) {
                    trace!("using configured route: {:?}", condition);
                    return service.call(req);
                }
            }

            trace!("using default route");
            self.default_route.call(req)
        }
    }
}
