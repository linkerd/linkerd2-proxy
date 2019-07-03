extern crate tower_discover;

use futures::Stream;
use http;
use indexmap::IndexMap;
// Import is used by WeightedIndex::sample.
#[allow(unused_imports)]
use rand::distributions::Distribution;
use rand::distributions::WeightedIndex;
use regex::Regex;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::iter::FromIterator;
use std::ops::Deref;
use std::sync::Arc;
use std::time::Duration;

use never::Never;

use super::retry::Budget;
use NameAddr;

#[derive(Clone)]
pub struct WeightedAddr {
    pub addr: NameAddr,
    pub weight: u32,
}

pub struct Routes {
    pub routes: Vec<(RequestMatch, Route)>,
    pub dst_overrides: Vec<WeightedAddr>,
}

/// Watches a destination's Routes.
///
/// The stream updates with all routes for the given destination. The stream
/// never ends and cannot fail.
pub trait GetRoutes {
    type Stream: Stream<Item = Routes, Error = Never>;

    fn get_routes(&self, dst: &NameAddr) -> Option<Self::Stream>;
}

/// Implemented by target types that may be combined with a Route.
pub trait WithRoute {
    type Output;

    fn with_route(self, route: Route) -> Self::Output;
}

/// Implemented by target types that can have their `NameAddr` destination
/// changed.
pub trait WithAddr {
    fn with_addr(self, addr: NameAddr) -> Self;
}

/// Implemented by target types that may have a `NameAddr` destination that
/// can be discovered via `GetRoutes`.
pub trait CanGetDestination {
    fn get_destination(&self) -> Option<&NameAddr>;
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
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
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
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A stack module that produces a Service that routes requests through alternate
/// middleware configurations
///
/// As the router's Stack is built, a destination is extracted from the stack's
/// target and it is used to get route profiles from ` GetRoutes` implemetnation.
///
/// Each route uses a shared underlying concrete dst router.  The concrete dst
/// router picks a concrete dst (NameAddr) from the profile's `dst_overrides` if
/// they exist, or uses the router's target's addr if no `dst_overrides` exist.
/// The concrete dst router uses the concrete dst as the target for the
/// underlying stack.
pub mod router {
    extern crate linkerd2_router as rt;

    use futures::{Async, Poll, Stream};
    use http;
    use std::hash::Hash;

    use never::Never;

    use dns;
    use proxy::Error;
    use svc;

    use super::*;

    // A router which routes based on the `dst_overrides` of the profile or, if
    // no `dst_overrdies` exist, on the router's target.
    type ConcreteRouter<T, Svc, B> =
        rt::Router<http::Request<B>, ConcreteDstRecognize<T>, rt::FixedMake<T, Svc>>;

    // A router which routes based on the "route" of the target.
    type RouteRouter<T, RT, Svc, B> =
        rt::Router<http::Request<B>, RouteRecognize<T>, rt::FixedMake<RT, Svc>>;

    pub fn layer<G, M, R, RBody, MBody>(
        suffixes: Vec<dns::Suffix>,
        get_routes: G,
        route_layer: svc::Builder<R>,
    ) -> Layer<G, M, R, RBody, MBody>
    where
        G: GetRoutes + Clone,
        R: Clone,
    {
        Layer {
            suffixes,
            get_routes,
            route_layer,
            default_route: Route::default(),
            _p: ::std::marker::PhantomData,
        }
    }

    #[derive(Debug)]
    pub struct Layer<G, M, R, RBody, MBody> {
        get_routes: G,
        route_layer: svc::Builder<R>,
        suffixes: Vec<dns::Suffix>,
        /// This is saved into a field so that the same `Arc`s are used and
        /// cloned, instead of calling `Route::default()` every time.
        default_route: Route,
        _p: ::std::marker::PhantomData<fn() -> (M, RBody, MBody)>,
    }

    #[derive(Debug)]
    pub struct MakeSvc<G, M, R, RBody, MBody> {
        inner: M,
        get_routes: G,
        route_layer: svc::Builder<R>,
        suffixes: Vec<dns::Suffix>,
        default_route: Route,
        _p: ::std::marker::PhantomData<fn(RBody, MBody)>,
    }

    /// The Service consists of a RouteRouter which routes over the route
    /// stack built by the `route_layer`.  The per-route stack is terminated by
    /// a shared `concrete_router`.  The `concrete_router` routes over the
    /// underlying stack and passes the concrete dst as the target.
    ///
    /// ```plain
    ///     +--------------+
    ///     |RouteRouter   | Target = t
    ///     +--------------+
    ///     |route_layer   | Target = t.withRoute(route)
    ///     +--------------+
    ///     |ConcreteRouter| Target = t
    ///     +--------------+
    ///     |inner         | Target = t.withAddr(concrete_dst)
    ///     +--------------+
    /// ```
    pub struct Service<G, T, R, S, RMk, M, RBody, MBody>
    where
        T: WithAddr + WithRoute + Clone + Eq + Hash,
        T::Output: Clone + Eq + Hash,
        M: rt::Make<T>,
        M::Value: svc::Service<http::Request<MBody>> + Clone,
        R: svc::Layer<S, Service = RMk>,
        RMk: rt::Make<T::Output>,
        RMk::Value: svc::Service<http::Request<RBody>> + Clone,
    {
        target: T,
        inner: M,
        route_layer: svc::Builder<R>,
        route_stream: Option<G>,
        concrete_router: Option<ConcreteRouter<T, M::Value, MBody>>,
        router: RouteRouter<T, T::Output, RMk::Value, RBody>,
        default_route: Route,
        _p: ::std::marker::PhantomData<S>,
    }

    #[derive(Clone)]
    pub struct RouteRecognize<T> {
        target: T,
        routes: Vec<(RequestMatch, Route)>,
        default_route: Route,
    }

    #[derive(Clone)]
    pub struct ConcreteDstRecognize<T> {
        target: T,
        dst_overrides: Vec<WeightedAddr>,
        // A weighted index of the `dst_overrides` weights.  This must only be
        // None if `dst_overrides` is empty.
        distribution: Option<WeightedIndex<u32>>,
    }

    impl<B, T> rt::Recognize<http::Request<B>> for RouteRecognize<T>
    where
        T: WithRoute + Clone,
        T::Output: Clone + Eq + Hash,
    {
        type Target = T::Output;

        fn recognize(&self, req: &http::Request<B>) -> Option<Self::Target> {
            for (ref condition, ref route) in &self.routes {
                if condition.is_match(&req) {
                    trace!("using configured route: {:?}", condition);
                    return Some(self.target.clone().with_route(route.clone()));
                }
            }

            trace!("using default route");
            Some(self.target.clone().with_route(self.default_route.clone()))
        }
    }

    impl<T> ConcreteDstRecognize<T> {
        fn new(target: T, dst_overrides: Vec<WeightedAddr>) -> Self {
            let distribution = Self::make_dist(&dst_overrides);
            ConcreteDstRecognize {
                target,
                dst_overrides,
                distribution,
            }
        }

        fn make_dist(dst_overrides: &Vec<WeightedAddr>) -> Option<WeightedIndex<u32>> {
            let mut weights = dst_overrides.iter().map(|dst| dst.weight).peekable();
            if weights.peek().is_none() {
                // Weights list is empty.
                None
            } else {
                Some(WeightedIndex::new(weights).expect("invalid weight distribution"))
            }
        }
    }

    impl<B, T> rt::Recognize<http::Request<B>> for ConcreteDstRecognize<T>
    where
        T: WithAddr + Clone + Eq + Hash,
    {
        type Target = T;

        fn recognize(&self, _req: &http::Request<B>) -> Option<Self::Target> {
            match self.distribution {
                Some(ref distribution) => {
                    let mut rng = rand::thread_rng();
                    let idx = distribution.sample(&mut rng);
                    let addr = self.dst_overrides[idx].addr.clone();
                    Some(self.target.clone().with_addr(addr))
                }
                None => Some(self.target.clone()),
            }
        }
    }

    impl<G, M, R, RBody, MBody> svc::Layer<M> for Layer<G, M, R, RBody, MBody>
    where
        G: GetRoutes + Clone,
        R: Clone,
    {
        type Service = MakeSvc<G, M, R, RBody, MBody>;

        fn layer(&self, inner: M) -> Self::Service {
            MakeSvc {
                inner,
                get_routes: self.get_routes.clone(),
                route_layer: self.route_layer.clone(),
                suffixes: self.suffixes.clone(),
                default_route: self.default_route.clone(),
                _p: ::std::marker::PhantomData,
            }
        }
    }

    impl<G, M, R, RBody, MBody> Clone for Layer<G, M, R, RBody, MBody>
    where
        G: Clone,
        R: Clone,
    {
        fn clone(&self) -> Self {
            Layer {
                suffixes: self.suffixes.clone(),
                get_routes: self.get_routes.clone(),
                route_layer: self.route_layer.clone(),
                default_route: self.default_route.clone(),
                _p: ::std::marker::PhantomData,
            }
        }
    }

    impl<T, G, M, R, RBody, MBody, MSvc, RMk, RSvc> svc::Service<T> for MakeSvc<G, M, R, RBody, MBody>
    where
        T: CanGetDestination + WithRoute + WithAddr + Eq + Hash + Clone,
        <T as WithRoute>::Output: Eq + Hash + Clone,
        M: rt::Make<T, Value = MSvc> + Clone,
        MSvc: svc::Service<http::Request<MBody>> + Clone,
        MSvc::Error: Into<Error>,
        G: GetRoutes,
        R: svc::Layer<svc::shared::Shared<ConcreteRouter<T, M::Value, MBody>>, Service = RMk>
            + Clone,
        RMk: rt::Make<<T as WithRoute>::Output, Value = RSvc> + Clone,
        RSvc: svc::Service<http::Request<RBody>> + Clone,
        RSvc::Error: Into<Error>,
    {
        type Response = Service<
            G::Stream,
            T,
            R,
            svc::shared::Shared<ConcreteRouter<T, M::Value, MBody>>,
            RMk,
            M,
            RBody,
            MBody,
        >;
        type Error = never::Never;
        type Future = futures::future::FutureResult<Self::Response, Self::Error>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            Ok(().into()) // always ready to make a Router
        }

        fn call(&mut self, target: T) -> Self::Future {
            let concrete_router = {
                // Initially there are no dst_overrides, so build a concrete router
                // with only the default target.
                let mut make = IndexMap::with_capacity(1);
                make.insert(target.clone(), self.inner.make(&target));

                let rec = ConcreteDstRecognize::new(target.clone(), Vec::new());
                rt::Router::new_fixed(rec, make)
            };

            let stack = self
                .route_layer
                .clone()
                .service(svc::shared(concrete_router.clone()));

            // Initially there are no routes, so build a route router with only
            // the default route.
            let default_route = target.clone().with_route(self.default_route.clone());

            let mut make = IndexMap::with_capacity(1);
            make.insert(default_route.clone(), stack.make(&default_route));

            let router = rt::Router::new_fixed(
                RouteRecognize {
                    target: target.clone(),
                    routes: Vec::new(),
                    default_route: self.default_route.clone(),
                },
                make,
            );

            // Initiate a stream to get route and dst_override updates for this
            // destination.
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

            futures::future::ok(Service {
                target,
                inner: self.inner.clone(),
                route_layer: self.route_layer.clone(),
                route_stream,
                router,
                concrete_router: Some(concrete_router),
                default_route: self.default_route.clone(),
                _p: ::std::marker::PhantomData,
            })
        }
    }

    impl<G, M, R, MBody, RBody> Clone for MakeSvc<G, M, R, MBody, RBody>
    where
        G: Clone,
        M: Clone,
        R: Clone,
    {
        fn clone(&self) -> Self {
            MakeSvc {
                inner: self.inner.clone(),
                get_routes: self.get_routes.clone(),
                route_layer: self.route_layer.clone(),
                suffixes: self.suffixes.clone(),
                default_route: self.default_route.clone(),
                _p: ::std::marker::PhantomData,
            }
        }
    }

    impl<G, T, R, RMk, M, RBody, MBody>
        Service<
            G,
            T,
            R,
            svc::shared::Shared<ConcreteRouter<T, M::Value, MBody>>,
            RMk,
            M,
            RBody,
            MBody,
        >
    where
        G: Stream<Item = Routes, Error = Never>,
        T: WithRoute + WithAddr + Eq + Hash + Clone,
        T::Output: Clone + Eq + Hash,
        R: svc::Layer<svc::shared::Shared<ConcreteRouter<T, M::Value, MBody>>, Service = RMk>
            + Clone,
        RMk: rt::Make<T::Output> + Clone,
        RMk::Value: svc::Service<http::Request<RBody>> + Clone,
        M: rt::Make<T> + Clone,
        M::Value: svc::Service<http::Request<MBody>> + Clone,
    {
        fn update_routes(&mut self, routes: Routes) {
            // We must build a new concrete router with a service for each
            // dst_override.  These services are created eagerly.  If a service
            // was present in the previous concrete router, we reuse that
            // service in the new concrete router rather than recreating it.
            let capacity = routes.dst_overrides.len() + 1;

            let mut make = IndexMap::with_capacity(capacity);
            let mut old_make = self
                .concrete_router
                .take()
                .expect("previous concrete dst router is missing")
                .into_make();

            let target_svc = old_make.remove(&self.target).unwrap_or_else(|| {
                error!("concrete dst router did not contain target dst");
                self.inner.make(&self.target)
            });
            make.insert(self.target.clone(), target_svc);

            for WeightedAddr { addr, .. } in &routes.dst_overrides {
                let target = self.target.clone().with_addr(addr.clone());
                let service = old_make
                    .remove(&target)
                    .unwrap_or_else(|| self.inner.make(&target));
                make.insert(target, service);
            }

            let concrete_router = rt::Router::new_fixed(
                ConcreteDstRecognize::new(self.target.clone(), routes.dst_overrides),
                make,
            );

            // We store the concrete_router directly in the Service struct so
            // that we can extract its services when its time to construct a
            // new concrete router.
            self.concrete_router = Some(concrete_router.clone());

            let stack = self
                .route_layer
                .clone()
                .service(svc::shared(concrete_router));

            let default_route = self.target.clone().with_route(self.default_route.clone());

            // Create a new fixed router router; we can eagerly make the
            // services and never expire the routes from the profile router
            // cache.
            let capacity = routes.routes.len() + 1;
            let mut make = IndexMap::with_capacity(capacity);
            make.insert(default_route.clone(), stack.make(&default_route));

            for (_, route) in &routes.routes {
                let route = self.target.clone().with_route(route.clone());
                let service = stack.make(&route);
                make.insert(route, service);
            }

            let router = rt::Router::new_fixed(
                RouteRecognize {
                    target: self.target.clone(),
                    routes: routes.routes,
                    default_route: self.default_route.clone(),
                },
                make,
            );

            self.router = router;
        }

        fn poll_route_stream(&mut self) -> Option<Async<Option<Routes>>> {
            self.route_stream
                .as_mut()
                .and_then(|ref mut s| s.poll().ok())
        }
    }

    impl<G, T, R, RMk, M, RBody, MBody, Svc> svc::Service<http::Request<RBody>>
        for Service<
            G,
            T,
            R,
            svc::shared::Shared<ConcreteRouter<T, M::Value, MBody>>,
            RMk,
            M,
            RBody,
            MBody,
        >
    where
        G: Stream<Item = Routes, Error = Never>,
        T: WithRoute + WithAddr + Eq + Hash + Clone,
        T::Output: Clone + Eq + Hash,
        R: svc::Layer<svc::shared::Shared<ConcreteRouter<T, M::Value, MBody>>, Service = RMk>
            + Clone,
        RMk: rt::Make<T::Output, Value = Svc> + Clone,
        M: rt::Make<T> + Clone,
        M::Value: svc::Service<http::Request<MBody>> + Clone,
        Svc: svc::Service<http::Request<RBody>> + Clone,
        Svc::Error: Into<Error>,
    {
        type Response = Svc::Response;
        type Error = Error;
        type Future = rt::ResponseFuture<
            http::Request<RBody>,
            RouteRecognize<T>,
            rt::FixedMake<T::Output, Svc>,
        >;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            while let Some(Async::Ready(Some(routes))) = self.poll_route_stream() {
                self.update_routes(routes);
            }

            Ok(Async::Ready(()))
        }

        fn call(&mut self, req: http::Request<RBody>) -> Self::Future {
            self.router.call(req)
        }
    }
}
