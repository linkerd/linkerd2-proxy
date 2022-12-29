use linkerd_app_core::{
    profiles::{self, Profile},
    proxy::api_resolve::ConcreteAddr,
    svc::{
        layer, NewCloneService, NewService, NewSpawnWatch, Oneshot, Param, Service, ServiceExt,
        UpdateWatch,
    },
    NameAddr,
};
use std::{
    collections::HashMap,
    marker::PhantomData,
    sync::Arc,
    task::{Context, Poll},
};
use tracing::{error, trace};

type Distribution = linkerd_distribute::Distribution<NameAddr>;
type Distribute<S> = linkerd_distribute::Distribute<NameAddr, S>;
type NewDistribute<S> = linkerd_distribute::NewDistribute<NameAddr, S>;

type Matches = Arc<[(profiles::http::RequestMatch, profiles::http::Route)]>;

/// A router that uses a per-route `Service` (with a fallback service when no
/// route is matched).
///
/// This router is similar to `linkerd_stack::NewRouter` and
/// `linkerd_cache::Cache` with a few differences:
///
/// * Routes are constructed eagerly as the profile updates;
/// * Routes are removed eagerly as the profile updates (i.e. there's no
///   idle-oriented eviction).
#[derive(Debug)]
pub struct NewRoute<N, R, U> {
    new_backend: N,
    route_layer: R,
    _marker: PhantomData<fn(U)>,
}

pub struct Update<T, U, N, S, L, R> {
    target: T,

    new_backend: N,
    backends: HashMap<NameAddr, S>,

    route_layer: L,
    matches: Matches,
    routes: HashMap<profiles::http::Route, R>,

    _marker: PhantomData<fn(U)>,
}

#[derive(Clone, Debug)]
pub struct Route<S> {
    matches: Matches,
    routes: HashMap<profiles::http::Route, S>, // TODO(ver) AHashMap?
}

// === impl NewRoute ===

impl<N, R: Clone, U> NewRoute<N, R, U> {
    pub fn layer(
        route_layer: R,
    ) -> impl layer::Layer<N, Service = NewSpawnWatch<Profile, Self>> + Clone {
        layer::mk(move |new_backend| {
            NewSpawnWatch::new(Self {
                new_backend,
                route_layer: route_layer.clone(),
                _marker: PhantomData,
            })
        })
    }
}

impl<T, U, N, R, S> NewService<T> for NewRoute<N, R, U>
where
    N: NewService<U> + Clone,
    R: layer::Layer<NewCloneService<Distribute<N::Service>>> + Clone,
    R::Service: NewService<(profiles::http::Route, T), Service = S>,
    S: Clone,
{
    type Service = Update<T, U, N, N::Service, R, S>;

    fn new_service(&self, target: T) -> Self::Service {
        Update {
            target,
            new_backend: self.new_backend.clone(),
            route_layer: self.route_layer.clone(),
            backends: HashMap::default(),
            matches: Arc::new([]),
            routes: HashMap::default(),
            _marker: PhantomData,
        }
    }
}

impl<N: Clone, R: Clone, U> Clone for NewRoute<N, R, U> {
    fn clone(&self) -> Self {
        Self {
            new_backend: self.new_backend.clone(),
            route_layer: self.route_layer.clone(),
            _marker: self._marker,
        }
    }
}

// === impl Route ===

impl<B, S> Service<http::Request<B>> for Route<S>
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
        // If the request matches a route, use the route's service.
        if let Some(route) = profiles::http::route_for_request(&self.matches, &req) {
            if let Some(svc) = self.routes.get(route).cloned() {
                trace!(?route, "Using route service");
                return svc.oneshot(req);
            }

            debug_assert!(false, "Route not found in cache");
            error!(?route, "Route not found in cache. This is a bug.");
        }

        todo!("handle no matching route");
    }
}

// === impl Shared ===

impl<S> Default for Route<S> {
    fn default() -> Self {
        Self {
            matches: Arc::new([]),
            routes: HashMap::new(),
        }
    }
}

// === impl Update ===

impl<T, U, N, L, R> UpdateWatch<Profile> for Update<T, U, N, N::Service, L, R>
where
    T: Param<profiles::LogicalAddr> + Clone,
    U: From<(ConcreteAddr, T)>,
    N: NewService<U>,
    N::Service: Clone,
    L: layer::Layer<NewCloneService<Distribute<N::Service>>>,
    L::Service: NewService<(profiles::http::Route, T), Service = R>,
    R: Clone,
{
    type Service = Route<R>;

    fn update(&mut self, profile: &Profile) -> Option<Self::Service> {
        let targets = profile
            .targets
            .iter()
            .map(|profiles::Target { addr, weight }| (addr.clone(), *weight))
            .collect::<HashMap<_, _>>();

        let changed_backends = self.update_backends(&targets);
        let changed_routes = *self.matches != profile.http_routes;
        if changed_backends || changed_routes {
            self.update_routes(&profile.http_routes, &targets);

            Some(Route {
                matches: self.matches.clone(),
                routes: self.routes.clone(),
            })
        } else {
            None
        }
    }
}

impl<T, U, N, L, R> Update<T, U, N, N::Service, L, R>
where
    T: Param<profiles::LogicalAddr> + Clone,
    U: From<(ConcreteAddr, T)>,
    N: NewService<U>,
    N::Service: Clone,
    L: layer::Layer<NewCloneService<Distribute<N::Service>>>,
    L::Service: NewService<(profiles::http::Route, T), Service = R>,
    R: Clone,
{
    fn update_backends<V>(&mut self, targets: &HashMap<NameAddr, V>) -> bool {
        let removed = {
            let init = self.backends.len();
            self.backends.retain(|addr, _| targets.contains_key(addr));
            init - self.backends.len()
        };

        if targets
            .iter()
            .all(|(addr, _)| self.backends.contains_key(addr))
        {
            return removed > 0;
        }

        self.backends.reserve(targets.len().max(1));
        for addr in targets.keys() {
            // Skip rebuilding targets we already have a stack for.
            if self.backends.contains_key(addr) {
                continue;
            }

            let backend = self
                .new_backend
                .new_service(U::from((ConcreteAddr(addr.clone()), self.target.clone())));
            self.backends.insert(addr.clone(), backend);
        }

        // TODO(ver) we should make it a requirement of the provider that there
        // is always at least one backend.
        if self.backends.is_empty() {
            let profiles::LogicalAddr(addr) = self.target.param();
            let backend = self
                .new_backend
                .new_service(U::from((ConcreteAddr(addr.clone()), self.target.clone())));
            self.backends.insert(addr, backend);
        }

        true
    }

    fn update_routes(
        &mut self,
        http_routes: &[(profiles::http::RequestMatch, profiles::http::Route)],
        targets: &HashMap<NameAddr, u32>,
    ) {
        let new_distribute: NewDistribute<N::Service> = self.backends.clone().into();

        self.matches = http_routes.iter().cloned().collect();

        self.routes = http_routes
            .iter()
            .map(|(_, r)| {
                // TODO(ver) targets should be provided by the route
                // configuration.
                let distribution = if targets.is_empty() {
                    let profiles::LogicalAddr(addr) = self.target.param();
                    Distribution::from(addr)
                } else {
                    Distribution::random_available(
                        targets.iter().map(|(addr, weight)| (addr.clone(), *weight)),
                    )
                    .expect("distribution must be valid")
                };

                let dist = NewCloneService::from(new_distribute.new_service(distribution));
                let new_route = self.route_layer.layer(dist);
                let svc = new_route.new_service((r.clone(), self.target.clone()));
                (r.clone(), svc)
            })
            .collect();
    }
}
