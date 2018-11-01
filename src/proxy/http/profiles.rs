#![allow(dead_code)]

extern crate tower_discover;

use futures::Stream;
use http;
use indexmap::IndexMap;
use regex::Regex;
use std::iter::FromIterator;
use std::sync::Arc;
use std::{error, fmt};

use transport::DnsNameAndPort;

pub trait CanGetDestination {
    fn get_destination(&self) -> Option<&DnsNameAndPort>;
}

pub type Routes = Vec<(RequestMatch, Route)>;

pub trait GetRoutes {
    type Stream: Stream<Item = Routes, Error = Error>;

    fn get_routes(&self, dst: &DnsNameAndPort) -> Option<Self::Stream>;
}

pub trait WithRoute {
    type Output;

    fn with_route(self, route: Route) -> Self::Output;
}

#[derive(Debug)]
pub enum Error {}

#[derive(Clone, Debug, Default)]
pub struct Route {
    labels: Arc<IndexMap<String, String>>,
    response_classes: Arc<Vec<ResponseClass>>,
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
            let mut pairs = label_iter.collect::<Vec<(String, String)>>();
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

    pub fn response_classes(&self) -> &Arc<Vec<ResponseClass>> {
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
            RequestMatch::All(ref matches) => {
                for ref m in matches {
                    if !m.is_match(req) {
                        return false;
                    }
                }
                true
            }
            RequestMatch::Any(ref matches) => {
                for ref m in matches {
                    if m.is_match(req) {
                        return true;
                    }
                }
                false
            }
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
            ResponseMatch::All(ref matches) => {
                for ref m in matches {
                    if !m.is_match(req) {
                        return false;
                    }
                }
                true
            }
            ResponseMatch::Any(ref matches) => {
                for ref m in matches {
                    if m.is_match(req) {
                        return true;
                    }
                }
                false
            }
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

    use svc;

    use super::*;

    pub fn layer<T, G, M, R>(get_routes: G, route_layer: R) -> Layer<G, M, R>
    where
        T: CanGetDestination + WithRoute + Clone,
        M: svc::Stack<T>,
        M::Value: Clone,
        G: GetRoutes + Clone,
        R: svc::Layer<
                <T as WithRoute>::Output,
                <T as WithRoute>::Output,
                svc::shared::Stack<M::Value>,
            >
            + Clone,
        R::Value: svc::Service,
    {
        Layer {
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
        _p: ::std::marker::PhantomData<fn() -> M>,
    }

    #[derive(Clone, Debug)]
    pub struct Stack<G, M, R = ()> {
        inner: M,
        get_routes: G,
        route_layer: R,
        default_route: Route,
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
        R::Value: svc::Service,
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
            >
            + Clone,
        R::Value: svc::Service,
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
            >
            + Clone,
        R::Value: svc::Service,
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

            let route_stream = target
                .get_destination()
                .and_then(|d| self.get_routes.get_routes(&d));

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
        R::Value: svc::Service,
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

    impl<G, T, R, B> svc::Service for Service<G, T, R>
    where
        G: Stream<Item = Routes, Error = super::Error>,
        T: WithRoute + Clone,
        R: svc::Stack<T::Output> + Clone,
        R::Value: svc::Service<Request = http::Request<B>>,
    {
        type Request = <R::Value as svc::Service>::Request;
        type Response = <R::Value as svc::Service>::Response;
        type Error = <R::Value as svc::Service>::Error;
        type Future = <R::Value as svc::Service>::Future;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            while let Some(Async::Ready(Some(routes))) = self.poll_route_stream() {
                self.update_routes(routes);
            }

            Ok(Async::Ready(()))
        }

        fn call(&mut self, req: Self::Request) -> Self::Future {
            for (ref condition, ref mut service) in &mut self.routes {
                if condition.is_match(&req) {
                    return service.call(req);
                }
            }

            self.default_route.call(req)
        }
    }
}
