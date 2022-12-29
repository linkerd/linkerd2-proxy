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
    marker::PhantomData,
    task::{Context, Poll},
};
use tokio::sync::watch;
use tracing::{error, trace, Instrument};

type Distribution = linkerd_distribute::Distribution<NameAddr>;
type Distribute<S> = linkerd_distribute::Distribute<NameAddr, S>;
type NewDistribute<S> = linkerd_distribute::NewDistribute<NameAddr, S>;

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
pub struct NewRouter<N, R, U> {
    new_backend: N,
    route_layer: R,
    _marker: PhantomData<fn(U)>,
}

#[derive(Clone, Debug)]
pub struct Router<S>(watch::Receiver<Shared<S>>);

#[derive(Debug)]
struct Shared<S> {
    matches: Vec<(RequestMatch, Route)>,
    routes: HashMap<Route, S>, // TODO(ver) AHashMap?
}

struct State<T, U, N, S, L, R> {
    target: T,

    new_backend: N,
    backends: HashMap<NameAddr, S>,

    route_layer: L,
    matches: Vec<(RequestMatch, Route)>,
    routes: HashMap<Route, R>,

    _marker: PhantomData<fn(U)>,
}

// === impl NewRouter ===

impl<N, R: Clone, U> NewRouter<N, R, U> {
    pub fn layer(route_layer: R) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |new_backend| Self {
            new_backend,
            route_layer: route_layer.clone(),
            _marker: PhantomData,
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
    R: layer::Layer<NewCloneService<Distribute<N::Service>>>,
    R: Clone + Send + 'static,
    R::Service: NewService<(Route, T), Service = S>,
    S: Clone + Send + Sync + 'static,
{
    type Service = Router<S>;

    fn new_service(&self, target: T) -> Self::Service {
        let mut profiles: profiles::Receiver = target.param();

        // Spawn a background task that updates the all routes and backends for the router.
        let mut state = State::new(target, self.new_backend.clone(), self.route_layer.clone());
        let (tx, rx) = watch::channel(
            state
                .update(&*profiles.borrow_and_update())
                .expect("initial update must produce a new state"),
        );
        tokio::spawn(
            state
                .run(profiles, tx)
                .instrument(tracing::debug_span!("httprouter")),
        );

        Router(rx)
    }
}

impl<N: Clone, R: Clone, U> Clone for NewRouter<N, R, U> {
    fn clone(&self) -> Self {
        Self {
            new_backend: self.new_backend.clone(),
            route_layer: self.route_layer.clone(),
            _marker: PhantomData,
        }
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

// === impl Shared ===

impl<S> Default for Shared<S> {
    fn default() -> Self {
        Self {
            matches: Vec::new(),
            routes: HashMap::new(),
        }
    }
}

// === impl State ===

impl<T, U, N, L, R> State<T, U, N, N::Service, L, R>
where
    T: Param<profiles::LogicalAddr> + Clone,
    U: From<(ConcreteAddr, T)>,
    N: NewService<U>,
    N::Service: Clone,
    L: layer::Layer<NewCloneService<Distribute<N::Service>>>,
    L::Service: NewService<(Route, T), Service = R>,
    R: Clone,
{
    fn new(target: T, new_backend: N, route_layer: L) -> Self {
        Self {
            target,
            new_backend,
            route_layer,
            backends: HashMap::default(),
            matches: Vec::new(),
            routes: HashMap::default(),
            _marker: PhantomData,
        }
    }

    async fn run(mut self, mut profiles: profiles::Receiver, tx: watch::Sender<Shared<R>>) {
        while profiles.changed().await.is_ok() {
            let profile = profiles.borrow_and_update();
            if let Some(shared) = self.update(&profile) {
                tracing::debug!("Publishing updated state");
                if tx.send(shared).is_err() {
                    return;
                }
            }
        }
    }

    fn update(&mut self, profile: &Profile) -> Option<Shared<R>> {
        let targets = profile
            .targets
            .iter()
            .map(|profiles::Target { addr, weight }| (addr.clone(), *weight))
            .collect::<HashMap<_, _>>();

        let changed_backends = self.update_backends(&targets);
        let changed_routes = *self.matches != profile.http_routes;
        if changed_backends || changed_routes {
            self.update_routes(&profile.http_routes, &targets);

            Some(Shared {
                matches: self.matches.clone(),
                routes: self.routes.clone(),
            })
        } else {
            None
        }
    }

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
        http_routes: &[(RequestMatch, Route)],
        targets: &HashMap<NameAddr, u32>,
    ) {
        let new_distribute: NewDistribute<N::Service> = self.backends.clone().into();

        self.matches = http_routes.to_vec();

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
