use futures::prelude::*;
use linkerd_app_core::{
    profiles::{
        self,
        http::{RequestMatch, Route},
        Profile,
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

struct Rebuild<T, U, N: NewService<U>, R, S> {
    target: T,
    new_backend: N,
    route_layer: R,
    targets: HashMap<NameAddr, u32>,
    backends: HashMap<NameAddr, N::Service>,
    distribute: NewCloneService<Distribute<N::Service>>,
    tx: watch::Sender<Shared<S>>,
    _marker: std::marker::PhantomData<fn(U)>,
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
    U: From<(ConcreteAddr, T)> + Send + 'static,
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

        let targets = profile
            .targets
            .iter()
            .map(|profiles::Target { addr, weight }| (addr.clone(), *weight))
            .collect::<HashMap<_, _>>();

        // Initial backends.
        // TODO(ver) use a different key type that is structured instead of
        // simply a name.
        let mut backends = HashMap::with_capacity(profile.targets.len().max(1));
        let n_backends = Rebuild::<T, U, N, R, S>::make_backends(
            &targets,
            &mut backends,
            &target,
            &self.new_backend,
        );
        tracing::debug!(backends = n_backends, "Built backends");

        // Initial distribution.
        let distribute = Rebuild::<T, U, N, R, S>::make_distribute(&targets, &backends, &target);

        // Initial routes.
        let routes = Rebuild::<T, U, N, R, S>::make_routes(
            &profile.http_routes,
            &self.route_layer,
            distribute.clone(),
            &target,
        );

        let (tx, rx) = watch::channel(Shared {
            matches: profile.http_routes.clone(),
            routes,
        });

        drop(profile);

        // Spawn background rebuilder task.
        let new_backend = self.new_backend.clone();
        let route_layer = self.route_layer.clone();
        tokio::spawn(
            Rebuild {
                targets,
                backends,
                new_backend: self.new_backend.clone(),
                route_layer: self.route_layer.clone(),
                distribute,
                target,
                tx,
                _marker: std::marker::PhantomData,
            }
            .rebuild(profiles)
            .in_current_span(),
        );

        Router(rx)
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

// === impl Rebuild ===

impl<T, U, N, R, S> Rebuild<T, U, N, R, S>
where
    T: Param<profiles::LogicalAddr>,
    T: Clone + Send + 'static,
    U: From<(ConcreteAddr, T)>,
    N: NewService<U> + Clone + Send + 'static,
    N::Service: Clone + Send + Sync + 'static,
    R: layer::Layer<NewCloneService<Distribute<N::Service>>>, // TODO NewDistribute<N::Service>,
    R: Clone + Send + 'static,
    R::Service: NewService<(Route, T), Service = S>,
    S: Send + Sync + 'static,
{
    async fn rebuild(mut self, mut profiles: profiles::Receiver) {
        while profiles.changed().await.is_ok() {
            let profile = profiles.borrow_and_update();
            let targets = profile
                .targets
                .iter()
                .map(|profiles::Target { addr, weight }| (addr.clone(), *weight))
                .collect::<HashMap<_, _>>();

            if targets != self.targets {
                tracing::debug!(backends = targets.len(), "Updated backends");

                // Clear out old backends.
                let removed_backends = {
                    let current_backends = self.backends.len();
                    self.backends.retain(|addr, _| targets.contains_key(addr));
                    current_backends - self.backends.len()
                };

                // Update `backends`.
                let added_backends = Self::make_backends(
                    &targets,
                    &mut self.backends,
                    &self.target,
                    &self.new_backend,
                );

                // Update `distribute`.
                self.distribute = Self::make_distribute(&targets, &self.backends, &self.target);
                tracing::debug!(
                    backends.added = added_backends,
                    backends.removed = removed_backends,
                    "Updated backends"
                );
            } else {
                // Skip updating the backends if the targets and weights are
                // unchanged.
                tracing::debug!("Targets and weights have not changed");
                return;
            }

            // Update routes.
            tracing::debug!(routes = profile.http_routes.len(), "Updating HTTP routes");
            let routes = Self::make_routes(
                &profile.http_routes,
                &self.route_layer,
                self.distribute.clone(),
                &self.target,
            );

            // Publish new shared state.
            let shared = Shared {
                matches: profile.http_routes.clone(),
                routes,
            };
            if self.tx.send(shared).is_err() {
                tracing::debug!("No services are listening for profile updates, shutting down");
                return;
            }
        }
    }

    fn make_backends(
        targets: &HashMap<NameAddr, u32>,
        backends: &mut HashMap<NameAddr, N::Service>,
        target: &T,
        new_backend: &N,
    ) -> usize {
        backends.reserve(targets.len().max(1));
        let mut new_backends = 0;
        for addr in targets.keys() {
            // Skip rebuilding targets we already have a stack for.
            if backends.contains_key(addr) {
                continue;
            }

            let backend =
                new_backend.new_service(U::from((ConcreteAddr(addr.clone()), target.clone())));
            backends.insert(addr.clone(), backend);
            new_backends += 1;
        }

        // TODO(ver) we should make it a requirement of the provider that there
        // is always at least one backend.
        if backends.is_empty() {
            let profiles::LogicalAddr(addr) = target.param();
            let backend =
                new_backend.new_service(U::from((ConcreteAddr(addr.clone()), target.clone())));
            backends.insert(addr, backend);
            return 1;
        }

        new_backends
    }

    fn make_distribute(
        targets: &HashMap<NameAddr, u32>,
        backends: &HashMap<NameAddr, N::Service>,
        target: &T,
    ) -> NewCloneService<Distribute<N::Service>> {
        // Update the distribution.
        // Create a stack that can distribute requests to the backends.
        let new_distribute = backends
            .iter()
            .map(|(addr, svc)| (addr.clone(), svc.clone()))
            .collect::<NewDistribute<_>>();

        // Build a single distribution service that will be shared across all routes.
        //
        // TODO(ver) Move this into the route stack so that each route's
        // distribution may vary.
        let distribution = if targets.is_empty() {
            let profiles::LogicalAddr(addr) = target.param();
            Distribution::from(addr)
        } else {
            Distribution::random_available(
                targets.iter().map(|(addr, weight)| (addr.clone(), *weight)),
            )
            .expect("distribution must be valid")
        };

        NewCloneService::from(new_distribute.new_service(distribution))
    }

    fn make_routes(
        http_routes: &[(RequestMatch, Route)],
        route_layer: &R,
        distribute: NewCloneService<Distribute<N::Service>>,
        target: &T,
    ) -> HashMap<Route, S> {
        let new_route = route_layer.layer(distribute);
        http_routes
            .iter()
            .map(|(_, r)| {
                let svc = new_route.new_service((r.clone(), target.clone()));
                (r.clone(), svc)
            })
            .collect()
    }
}
