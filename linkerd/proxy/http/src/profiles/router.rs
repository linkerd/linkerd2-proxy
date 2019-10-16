use super::recognize::{ConcreteDstRecognize, RouteRecognize};
use super::{CanGetDestination, GetRoutes, Route, Routes, WeightedAddr, WithAddr, WithRoute};
use futures::{Async, Poll, Stream};
use http;
use indexmap::IndexMap;
use linkerd2_dns as dns;
use linkerd2_error::{Error, Never};
use linkerd2_router as rt;
use linkerd2_stack::Shared;
use std::hash::Hash;
use tracing::{debug, error};

// A router which routes based on the `dst_overrides` of the profile or, if
// no `dst_overrdies` exist, on the router's target.
type ConcreteRouter<Target, Svc, Body> =
    rt::Router<http::Request<Body>, ConcreteDstRecognize<Target>, rt::FixedMake<Target, Svc>>;

// A router which routes based on the "route" of the target.
type RouteRouter<Target, RouteTarget, Svc, Body> =
    rt::Router<http::Request<Body>, RouteRecognize<Target>, rt::FixedMake<RouteTarget, Svc>>;

pub fn layer<G, Inner, RouteLayer, RouteBody, InnerBody>(
    suffixes: Vec<dns::Suffix>,
    get_routes: G,
    route_layer: RouteLayer,
) -> Layer<G, Inner, RouteLayer, RouteBody, InnerBody>
where
    G: GetRoutes + Clone,
    RouteLayer: Clone,
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
pub struct Layer<G, Inner, RouteLayer, RouteBody, InnerBody> {
    get_routes: G,
    route_layer: RouteLayer,
    suffixes: Vec<dns::Suffix>,
    /// This is saved into a field so that the same `Arc`s are used and
    /// cloned, instead of calling `Route::default()` every time.
    default_route: Route,
    _p: ::std::marker::PhantomData<fn() -> (Inner, RouteBody, InnerBody)>,
}

#[derive(Debug)]
pub struct MakeSvc<G, Inner, RouteLayer, RouteBody, InnerBody> {
    inner: Inner,
    get_routes: G,
    route_layer: RouteLayer,
    suffixes: Vec<dns::Suffix>,
    default_route: Route,
    _p: ::std::marker::PhantomData<fn(RouteBody, InnerBody)>,
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
pub struct Service<RouteStream, Target, RouteLayer, RouteMake, Inner, RouteBody, InnerBody>
where
    Target: WithAddr + WithRoute + Clone + Eq + Hash,
    Target::Output: Clone + Eq + Hash,
    Inner: rt::Make<Target>,
    Inner::Value: tower::Service<http::Request<InnerBody>> + Clone,
    RouteLayer: tower::layer::Layer<
        Shared<ConcreteRouter<Target, Inner::Value, InnerBody>>,
        Service = RouteMake,
    >,
    RouteMake: rt::Make<Target::Output>,
    RouteMake::Value: tower::Service<http::Request<RouteBody>> + Clone,
{
    target: Target,
    inner: Inner,
    route_layer: RouteLayer,
    route_stream: Option<RouteStream>,
    concrete_router: Option<ConcreteRouter<Target, Inner::Value, InnerBody>>,
    router: RouteRouter<Target, Target::Output, RouteMake::Value, RouteBody>,
    default_route: Route,
}

impl<G, Inner, RouteLayer, RouteBody, InnerBody> tower::layer::Layer<Inner>
    for Layer<G, Inner, RouteLayer, RouteBody, InnerBody>
where
    G: GetRoutes + Clone,
    RouteLayer: Clone,
{
    type Service = MakeSvc<G, Inner, RouteLayer, RouteBody, InnerBody>;

    fn layer(&self, inner: Inner) -> Self::Service {
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

impl<G, Inner, RouteLayer, RouteBody, InnerBody> Clone
    for Layer<G, Inner, RouteLayer, RouteBody, InnerBody>
where
    G: Clone,
    RouteLayer: Clone,
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

impl<G, Inner, RouteLayer, RouteBody, InnerBody, Target, RouteSvc> tower::Service<Target>
    for MakeSvc<G, Inner, RouteLayer, RouteBody, InnerBody>
where
    G: GetRoutes,
    Target: CanGetDestination + WithRoute + WithAddr + Eq + Hash + Clone,
    <Target as WithRoute>::Output: Eq + Hash + Clone,
    Inner: rt::Make<Target> + Clone,
    Inner::Value: tower::Service<http::Request<InnerBody>> + Clone,
    <Inner::Value as tower::Service<http::Request<InnerBody>>>::Error: Into<Error>,
    RouteLayer:
        tower::layer::Layer<Shared<ConcreteRouter<Target, Inner::Value, InnerBody>>> + Clone,
    RouteLayer::Service: rt::Make<<Target as WithRoute>::Output, Value = RouteSvc> + Clone,
    RouteSvc: tower::Service<http::Request<RouteBody>> + Clone,
    RouteSvc::Error: Into<Error>,
{
    type Response =
        Service<G::Stream, Target, RouteLayer, RouteLayer::Service, Inner, RouteBody, InnerBody>;
    type Error = Never;
    type Future = futures::future::FutureResult<Self::Response, Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(().into()) // always ready to make a Router
    }

    fn call(&mut self, target: Target) -> Self::Future {
        let concrete_router = {
            // Initially there are no dst_overrides, so build a concrete router
            // with only the default target.
            let mut make = IndexMap::with_capacity(1);
            make.insert(target.clone(), self.inner.make(&target));

            let rec = ConcreteDstRecognize::new(target.clone(), Vec::new());
            rt::Router::new_fixed(rec, make)
        };

        let concrete_stack = self.route_layer.layer(Shared::new(concrete_router.clone()));

        // Initially there are no routes, so build a route router with only
        // the default route.
        let router = {
            let default_route = target.clone().with_route(self.default_route.clone());
            let stack = rt::Make::make(&concrete_stack, &default_route);

            let mut make = IndexMap::with_capacity(1);
            make.insert(default_route.clone(), stack);

            let recognize = RouteRecognize::new(target.clone(), vec![], self.default_route.clone());
            rt::Router::new_fixed(recognize, make)
        };

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
        })
    }
}

impl<G, Inner, RouteLayer, InnerBody, RouteBody> Clone
    for MakeSvc<G, Inner, RouteLayer, InnerBody, RouteBody>
where
    G: Clone,
    Inner: Clone,
    RouteLayer: Clone,
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

impl<RouteStream, Target, RouteLayer, RouteMake, Inner, RouteBody, InnerBody>
    Service<RouteStream, Target, RouteLayer, RouteMake, Inner, RouteBody, InnerBody>
where
    RouteStream: Stream<Item = Routes, Error = Never>,
    Target: WithRoute + WithAddr + Eq + Hash + Clone,
    Target::Output: Clone + Eq + Hash,
    RouteLayer: tower::layer::Layer<
            Shared<ConcreteRouter<Target, Inner::Value, InnerBody>>,
            Service = RouteMake,
        > + Clone,
    RouteMake: rt::Make<Target::Output> + Clone,
    RouteMake::Value: tower::Service<http::Request<RouteBody>> + Clone,
    Inner: rt::Make<Target> + Clone,
    Inner::Value: tower::Service<http::Request<InnerBody>> + Clone,
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

        let stack = self.route_layer.layer(Shared::new(concrete_router));

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
            RouteRecognize::new(
                self.target.clone(),
                routes.routes,
                self.default_route.clone(),
            ),
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

impl<RouteStream, Target, RouteLayer, RouteMake, Inner, RouteBody, InnerBody, RouteSvc>
    tower::Service<http::Request<RouteBody>>
    for Service<RouteStream, Target, RouteLayer, RouteMake, Inner, RouteBody, InnerBody>
where
    RouteStream: Stream<Item = Routes, Error = Never>,
    Target: WithRoute + WithAddr + Eq + Hash + Clone,
    Target::Output: Clone + Eq + Hash,
    RouteLayer: tower::layer::Layer<
            Shared<ConcreteRouter<Target, Inner::Value, InnerBody>>,
            Service = RouteMake,
        > + Clone,
    RouteMake: rt::Make<Target::Output, Value = RouteSvc> + Clone,
    Inner: rt::Make<Target> + Clone,
    Inner::Value: tower::Service<http::Request<InnerBody>> + Clone,
    RouteSvc: tower::Service<http::Request<RouteBody>> + Clone,
    RouteSvc::Error: Into<Error>,
{
    type Response = RouteSvc::Response;
    type Error = Error;
    type Future = rt::ResponseFuture<
        http::Request<RouteBody>,
        RouteRecognize<Target>,
        rt::FixedMake<Target::Output, RouteSvc>,
    >;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        while let Some(Async::Ready(Some(routes))) = self.poll_route_stream() {
            self.update_routes(routes);
        }

        Ok(Async::Ready(()))
    }

    fn call(&mut self, req: http::Request<RouteBody>) -> Self::Future {
        self.router.call(req)
    }
}
