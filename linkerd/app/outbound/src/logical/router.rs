use futures::{future, prelude::*};
use linkerd_app_core::{
    profiles::Profile,
    proxy::api_resolve::ConcreteAddr,
    svc::{layer, NewService, NewSpawnWatch, Oneshot, Param, Service, ServiceExt, UpdateWatch},
    Error, NameAddr,
};
use linkerd_client_policy::MatchRoute;
use std::{
    collections::HashMap,
    fmt,
    hash::Hash,
    marker::PhantomData,
    task::{Context, Poll},
};
use tracing::{error, trace};

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
pub struct Route<M, K, S> {
    matches: M,
    routes: HashMap<K, S>, // TODO(ver) AHashMap?
}

#[derive(Debug, thiserror::Error)]
#[error("no route for request")]
pub struct NoRouteForRequest(());

// === impl NewRoute ===

impl<N, R: Clone, U, M, K> NewRoute<N, R, U, M, K>
where
    M: Default + Eq + Clone + Send + Sync + 'static,
    for<'a> &'a M: IntoIterator<Item = &'a K>,
{
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
    M: Default,
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

impl<M: Default, K, S> Default for Route<M, K, S> {
    fn default() -> Self {
        Self {
            matches: Default::default(),
            routes: HashMap::new(),
        }
    }
}

impl<M, S, Req> Service<Req> for Route<M, M::Route, S>
where
    S: Service<Req> + Clone,
    S::Error: Into<Error>,
    M: MatchRoute<Req> + Eq + Clone,
    M::Route: Hash + Eq + fmt::Debug,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::Either<
        future::MapErr<Oneshot<S, Req>, fn(S::Error) -> Error>,
        future::Ready<Result<S::Response, Error>>,
    >;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        // TODO(ver) figure out how backpressure should work here. Ideally, we
        // should only advertise readiness when at least one backend is ready.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Req) -> Self::Future {
        // If the request matches a route, use the route's service.
        if let Some(route) = self.matches.match_route(&req) {
            if let Some(svc) = self.routes.get(route).cloned() {
                trace!(?route, "Using route service");
                return future::Either::Left(svc.oneshot(req).map_err(Into::into));
            }

            debug_assert!(false, "Route not found in cache");
            error!(?route, "Route not found in cache. This is a bug.");
        }

        tracing::debug!("No route for request");
        future::Either::Right(future::ready(Err(NoRouteForRequest(()).into())))
    }
}

// === impl Update ===

impl<T, U, N, L, M, K, R> UpdateWatch<Profile> for Update<T, U, N, N::Service, L, M, K, R>
where
    T: Clone + Send + Sync + 'static,
    Profile: Param<M>,
    U: From<(ConcreteAddr, T)> + 'static,
    N: NewService<U> + Send + Sync + 'static,
    N::Service: Clone + Send + Sync + 'static,
    L: layer::Layer<NewDistribute<N::Service>> + Send + Sync + 'static,
    L::Service: NewService<(K, T), Service = R>,
    R: Clone + Send + Sync + 'static,
    M: Default + Eq + Clone + Send + Sync + 'static,
    for<'a> &'a M: IntoIterator<Item = &'a K>,
    K: Hash + Eq + Clone + Send + Sync + 'static,
{
    type Service = Route<M, K, R>;

    fn update(&mut self, profile: &Profile) -> Option<Self::Service> {
        let changed_backends = self.update_backends(&profile.backend_addrs);
        let routes = profile.param();
        let changed_routes = self.route.matches != routes;
        if changed_backends || changed_routes {
            self.update_routes(&routes);

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
    T: Clone,
    U: From<(ConcreteAddr, T)>,
    N: NewService<U>,
    N::Service: Clone,
    L: layer::Layer<NewDistribute<N::Service>>,
    L::Service: NewService<(K, T), Service = R>,
    R: Clone,
    M: Eq + Clone,
    for<'a> &'a M: IntoIterator<Item = &'a K>,
    K: Hash + Eq + Clone,
{
    fn update_backends(&mut self, addrs: &ahash::AHashSet<NameAddr>) -> bool {
        dbg!(&addrs);
        let removed = {
            let init = self.backends.len();
            self.backends.retain(|addr, _| addrs.contains(addr));
            init - self.backends.len()
        };

        // We just removed all backends that aren't in the new addrs, so we
        // we can skip further processing by comparing their lengths.
        debug_assert!(addrs.len() >= self.backends.len());
        if addrs.len() == self.backends.len() {
            return removed > 0;
        }

        self.backends.reserve(addrs.len());
        for addr in addrs {
            // Skip rebuilding targets we already have a stack for.
            if self.backends.contains_key(addr) {
                continue;
            }

            let backend = self
                .new_backend
                .new_service(U::from((ConcreteAddr(addr.clone()), self.target.clone())));
            self.backends.insert(addr.clone(), backend);
        }

        true
    }

    fn update_routes(&mut self, matches: &M) {
        let new_distribute: NewDistribute<N::Service> = self.backends.clone().into();
        self.route = Route {
            matches: matches.clone(),
            routes: matches
                .into_iter()
                .map(|route| {
                    let new_route = self.route_layer.layer(new_distribute.clone());
                    let svc = new_route.new_service((route.clone(), self.target.clone()));
                    (route.clone(), svc)
                })
                .collect(),
        };
    }
}
