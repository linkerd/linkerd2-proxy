use futures::prelude::*;
use linkerd_app_core::{
    profiles::{
        self,
        http::{RequestMatch, Route},
    },
    proxy::api_resolve::ConcreteAddr,
    svc::{layer, NewCloneService, NewService, Oneshot, Param, Service, ServiceExt},
    NameAddr,
};
use std::{
    collections::HashMap,
    task::{Context, Poll},
};
use tokio::sync::watch;
use tracing::{error, trace, Instrument};

type Distribution = linkerd_distribute::Distribution<NameAddr>;
type NewDistribute<S> = linkerd_distribute::NewDistribute<NameAddr, S>;
type Distribute<S> = linkerd_distribute::Distribute<NameAddr, S>;

/// A router that uses a per-route `Service` (with a fallback service when no
/// route is matched).
///
/// This router is similar to `linkerd_stack::NewRouter` and
/// `linkerd_cache::Cache` with a few differences:
///
/// * Routes are constructed eagerly as the profile updates;
/// * Routes are removed eagerly as the profile updates (i.e. there's no
///   idle-oriented eviction).
#[derive(Clone, Debug)]
pub struct NewRouter<N, R, U> {
    new_backend: N,
    route_layer: R,
    _marker: std::marker::PhantomData<fn(U)>,
}

#[derive(Clone, Debug)]
pub struct Router<S>(watch::Receiver<Shared<S>>);

#[derive(Debug)]
struct Shared<S> {
    matches: Vec<(RequestMatch, Route)>,
    routes: HashMap<Route, S>, // TODO(ver) AHashMap?
}

// === impl NewRouter ===

impl<N, R: Clone, U> NewRouter<N, R, U> {
    pub fn layer(route_layer: R) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |new_backend| Self {
            new_backend,
            route_layer: route_layer.clone(),
            _marker: std::marker::PhantomData,
        })
    }
}

impl<T, U, N, R, S> NewService<T> for NewRouter<N, R, U>
where
    T: Param<profiles::LogicalAddr> + Param<profiles::Receiver>,
    T: Clone + Send + 'static,
    U: From<(ConcreteAddr, T)>,
    N: NewService<U> + Clone + Send + 'static,
    N::Service: Clone + Send + Sync + 'static,
    R: layer::Layer<NewCloneService<Distribute<N::Service>>>, // TODO NewDistribute<N::Service>,
    R: Clone + Send + 'static,
    R::Service: NewService<(Route, T), Service = S>,
    S: Send + Sync + 'static,
{
    type Service = Router<S>;

    fn new_service(&self, target: T) -> Self::Service {
        // Spawn a background task that watches for profile updates and, when a
        // change is necessary, rebuilds stacks:
        //
        // 1. Maintain a cache of backend backend services (i.e., load
        //    balancers). These services are shared across all routes and
        //    therefor must be cloneable (i.e., buffered).
        // 2.
        // 3. Publish these stacks so that they may be used

        let mut profiles: profiles::Receiver = target.param();
        // Build the initial stacks by checking the profile.
        let profile = profiles.borrow_and_update();

        // TODO(ver) use a different key type that is structured instead of
        // simply a name.
        let mut backends = HashMap::with_capacity(profile.targets.len().max(1));
        Self::mk_backends(&mut backends, &self.new_backend, &profile.targets, &target);

        // Create a stack that can distribute requests to the backends.
        let distribute = Self::mk_distribute(&backends, &profile.targets, &target);
        let routes = Self::mk_routes(distribute, &self.route_layer, &profile.http_routes, &target);

        let (tx, rx) = watch::channel(Shared {
            matches: profile.http_routes.clone(),
            routes,
        });

        drop(profile);

        let new_backend = self.new_backend.clone();
        let route_layer = self.route_layer.clone();
        let span = tracing::debug_span!("router", target = %Param::<profiles::LogicalAddr>::param(&target));
        tokio::spawn(
            async move {
                while profiles.changed().await.is_ok() {
                    let profile = profiles.borrow_and_update();
                    // Update `backends`.
                    // TODO(eliza): can we avoid doing this if the backends haven't
                    // changed at all (e.g., handle updates that only change the
                    // routes?)
                    tracing::debug!(backends = profile.targets.len(), "Updating backends");
                    backends.clear();
                    Self::mk_backends(&mut backends, &new_backend, &profile.targets, &target);

                    // New distribution.
                    // TODO(eliza): if the backends and weights haven't changed, it
                    // would be nice to avoid rebuilding the distributor...
                    let distribute = Self::mk_distribute(&backends, &profile.targets, &target);

                    // New routes.
                    // TODO(eliza): if the backends and weights haven't changed, it
                    // would be nice to avoid rebuilding the distributor...
                    tracing::debug!(routes = profile.http_routes.len(), "Updating HTTP routes");
                    let routes =
                        Self::mk_routes(distribute, &route_layer, &profile.http_routes, &target);

                    // Publish new shared state.
                    let shared = Shared {
                        matches: profile.http_routes.clone(),
                        routes,
                    };
                    if tx.send(shared).is_err() {
                        tracing::debug!(
                            "No services are listening for profile updates, shutting down"
                        );
                        return;
                    }
                }

                tracing::debug!("Profile watch has closed, shutting down");
            }
            .instrument(span),
        );

        Router(rx)
    }
}

impl<U, N, R> NewRouter<N, R, U>
where
    N: NewService<U> + Clone,
    N::Service: Clone + Send + Sync + 'static,
{
    fn mk_backends<T>(
        backends: &mut HashMap<NameAddr, N::Service>,
        new_backend: &N,
        targets: &[profiles::Target],
        target: &T,
    ) where
        U: From<(ConcreteAddr, T)>,
        T: Param<profiles::LogicalAddr> + Clone,
    {
        backends.reserve(targets.len().max(1));
        for t in targets.iter() {
            let addr = t.addr.clone();
            let backend =
                new_backend.new_service(U::from((ConcreteAddr(addr.clone()), target.clone())));
            backends.insert(addr.clone(), backend);
        }

        // TODO(ver) we should make it a requirement of the provider that there
        // is always at least one backend.
        if backends.is_empty() {
            let profiles::LogicalAddr(addr) = target.param();
            let backend =
                new_backend.new_service(U::from((ConcreteAddr(addr.clone()), target.clone())));
            backends.insert(addr, backend);
        }
    }

    fn mk_distribute<T>(
        backends: &HashMap<NameAddr, N::Service>,
        targets: &[profiles::Target],
        target: &T,
    ) -> Distribute<N::Service>
    where
        T: Param<profiles::LogicalAddr>,
    {
        // Create a stack that can distribute requests to the backends.
        let new_distribute = NewDistribute::new(
            backends
                .iter()
                .map(|(addr, svc)| (addr.clone(), svc.clone())),
        );

        // Build a single distribution service that will be shared across all routes.
        //
        // TODO(ver) Move this into the route stack so that each route's
        // distribution may vary.
        let distribution = if targets.is_empty() {
            let profiles::LogicalAddr(addr) = target.param();
            std::iter::once((addr, 1)).collect::<Distribution>()
        } else {
            targets
                .iter()
                .cloned()
                .map(|profiles::Target { addr, weight }| (addr, weight))
                .collect::<Distribution>()
        };
        new_distribute.new_service(distribution)
    }

    fn mk_routes<S, T>(
        distribute: Distribute<N::Service>,
        route_layer: &R,
        http_routes: &[(RequestMatch, Route)],
        target: &T,
    ) -> HashMap<Route, S>
    where
        T: Clone,
        U: From<(ConcreteAddr, T)>,
        R: layer::Layer<NewCloneService<Distribute<N::Service>>>, // TODO NewDistribute<N::Service>,
        R::Service: NewService<(Route, T), Service = S>,
        S: Send + Sync + 'static,
    {
        let new_route = route_layer.layer(NewCloneService::from(distribute));
        http_routes
            .iter()
            .map(|(_, r)| {
                let svc = new_route.new_service((r.clone(), target.clone()));
                (r.clone(), svc)
            })
            .collect()
    }
}

// === impl Router ===

impl<B, S> Service<http::Request<B>> for Router<S>
where
    S: Service<http::Request<B>> + Clone,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Oneshot<S, http::Request<B>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        // TODO(ver) figure out how backpressure should work here. Ideally, we
        // should only advertise readiness when at least one backend is ready.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        let Shared { matches, routes } = &*self.0.borrow();

        // If the request matches a route, use the route's service.
        if let Some(route) = profiles::http::route_for_request(matches, &req) {
            if let Some(svc) = routes.get(route).cloned() {
                trace!(?route, "Using route service");
                return svc.oneshot(req);
            }

            debug_assert!(false, "Route not found in cache");
            error!(?route, "Route not found in cache. This is a bug.");
        }

        todo!("handle no matching route");
    }
}
