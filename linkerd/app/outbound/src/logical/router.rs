use linkerd_app_core::{
    profiles::{self, Profile},
    proxy::api_resolve::ConcreteAddr,
    svc::{layer, NewService, NewSpawnWatch, Oneshot, Param, Service, ServiceExt, UpdateWatch},
    NameAddr,
};
use std::{
    collections::HashMap,
    fmt,
    hash::Hash,
    marker::PhantomData,
    sync::Arc,
    task::{Context, Poll},
};
use tracing::{error, trace};

pub(crate) type Matches<M, K> = Arc<[(M, K)]>;
pub(crate) type Distribution = linkerd_distribute::Distribution<NameAddr>;
type NewDistribute<S> = linkerd_distribute::NewDistribute<NameAddr, S>;

/// A router that uses a per-route `Service` (with a fallback service when no
/// route is matched).
///
/// This router is similar to `linkerd_stack::NewRouter` and
/// `linkerd_cache::Cache` with a few differences:
///
/// * Routes are constructed eagerly as the profile updates by a background
///   task;
/// * Routes are removed eagerly as the profile updates (i.e. there's no
///   idle-oriented eviction).
#[derive(Debug)]
pub struct NewRoute<N, R, U, M, K> {
    new_backend: N,
    route_layer: R,
    _marker: PhantomData<fn((U, M, K))>,
}

pub struct Update<InT, OutT, N, S, L, M, K, R> {
    target: InT,

    new_backend: N,
    backends: HashMap<NameAddr, S>,

    route_layer: L,
    route: Route<M, K, R>,

    _marker: PhantomData<fn(OutT)>,
}

/// The [`Service`] constructed by [`NewRoute`].
#[derive(Clone, Debug)]
pub struct Route<M, R, S> {
    matches: Matches<M, R>,
    routes: HashMap<R, S>, // TODO(ver) AHashMap?
}

/// A type which can match a request to a route.
pub trait MatchRoute<Req>: Eq + Sized {
    /// Given a list of (route matcher, key) pairs and a request, returns the
    /// key matching this request according to a route matching strategy.
    ///
    /// If no route matches the request, this method returns `None`.
    ///
    /// Depending on the route matching strategy used by this type, the returned
    /// route may be the first route to match the request (ServiceProfile
    /// style), or the most specific route to match the request (HTTPRoute style).
    fn match_route<'matches, K>(
        matches: &'matches Matches<Self, K>,
        req: &Req,
    ) -> Option<&'matches K>;
}

// === impl NewRoute ===

impl<N, R: Clone, U, M, K> NewRoute<N, R, U, M, K> {
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

impl<T, U, N, R, M, K, S> NewService<T> for NewRoute<N, R, U, M, K>
where
    N: NewService<U> + Clone,
    R: layer::Layer<NewDistribute<N::Service>> + Clone,
    R::Service: NewService<(K, T), Service = S>,
    S: Clone,
    K: Clone,
{
    type Service = Update<T, U, N, N::Service, R, M, K, S>;

    fn new_service(&self, target: T) -> Self::Service {
        Update {
            target,
            new_backend: self.new_backend.clone(),
            route_layer: self.route_layer.clone(),
            backends: HashMap::default(),
            route: Route::default(),
            _marker: PhantomData,
        }
    }
}

impl<N: Clone, R: Clone, U, M, K> Clone for NewRoute<N, R, U, M, K> {
    fn clone(&self) -> Self {
        Self {
            new_backend: self.new_backend.clone(),
            route_layer: self.route_layer.clone(),
            _marker: self._marker,
        }
    }
}

// === impl Route ===

impl<M, K, S> Default for Route<M, K, S> {
    fn default() -> Self {
        Self {
            matches: Arc::new([]),
            routes: HashMap::new(),
        }
    }
}

impl<M, K, S, Req> Service<Req> for Route<M, K, S>
where
    S: Service<Req> + Clone,
    M: MatchRoute<Req>,
    K: Hash + Eq + fmt::Debug,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Oneshot<S, Req>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        // TODO(ver) figure out how backpressure should work here. Ideally, we
        // should only advertise readiness when at least one backend is ready.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Req) -> Self::Future {
        // If the request matches a route, use the route's service.
        if let Some(route) = M::match_route(&self.matches, &req) {
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

// === impl Update ===

impl<T, U, N, L, M, K, R> UpdateWatch<Profile> for Update<T, U, N, N::Service, L, M, K, R>
where
    T: Param<profiles::LogicalAddr> + Clone + Send + Sync + 'static,
    Profile: Param<Matches<M, K>>,
    U: From<(ConcreteAddr, T)> + 'static,
    N: NewService<U> + Send + Sync + 'static,
    N::Service: Clone + Send + Sync + 'static,
    L: layer::Layer<NewDistribute<N::Service>> + Send + Sync + 'static,
    L::Service: NewService<(K, T), Service = R>,
    R: Clone + Send + Sync + 'static,
    M: Eq + Clone + Send + Sync + 'static,
    K: Hash + Eq + Clone + Send + Sync + 'static,
{
    type Service = Route<M, K, R>;

    fn update(&mut self, profile: &Profile) -> Option<Self::Service> {
        let changed_backends = self.update_backends(&profile.target_addrs);
        let routes = profile.param();
        let changed_routes = self.route.matches != routes;
        if changed_backends || changed_routes {
            self.update_routes(&*routes);

            Some(Route {
                matches: self.route.matches.clone(),
                routes: self.route.routes.clone(),
            })
        } else {
            None
        }
    }
}

impl<T, U, N, L, M, K, R> Update<T, U, N, N::Service, L, M, K, R>
where
    T: Param<profiles::LogicalAddr> + Clone,
    U: From<(ConcreteAddr, T)>,
    N: NewService<U>,
    N::Service: Clone,
    L: layer::Layer<NewDistribute<N::Service>>,
    L::Service: NewService<(K, T), Service = R>,
    R: Clone,
    M: Clone,
    K: Hash + Eq + Clone,
{
    fn update_backends(&mut self, target_addrs: &ahash::AHashSet<NameAddr>) -> bool {
        let removed = {
            let init = self.backends.len();
            self.backends.retain(|addr, _| target_addrs.contains(addr));
            init - self.backends.len()
        };

        if target_addrs
            .iter()
            .all(|addr| self.backends.contains_key(addr))
            // Don't return early if we still need to populate the default backend!
            && !target_addrs.is_empty()
        {
            return removed > 0;
        }

        self.backends.reserve(target_addrs.len().max(1));
        for addr in target_addrs {
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

    fn update_routes(&mut self, routes: &[(M, K)]) {
        let new_distribute: NewDistribute<N::Service> = self.backends.clone().into();
        self.route = Route {
            matches: routes.iter().cloned().collect(),
            routes: routes
                .iter()
                .map(|(_, route)| {
                    let new_route = self.route_layer.layer(new_distribute.clone());
                    let svc = new_route.new_service((route.clone(), self.target.clone()));
                    (route.clone(), svc)
                })
                .collect(),
        };
    }
}
