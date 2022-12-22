use super::{RequestMatch, Route};
use crate::{LogicalAddr, NewDistribute, Profile};
use futures::{future::Either, prelude::*};
use linkerd_error::Error;
use linkerd_proxy_api_resolve::ConcreteAddr;
use linkerd_stack::{layer, NewService, Oneshot, Param, Service, ServiceExt};
use std::{
    collections::{hash_map, HashMap, HashSet},
    task::{Context, Poll},
};
use tokio::sync::watch;
use tracing::{debug, error, trace};

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
pub struct NewServiceRouter<N, R, U> {
    new_concrete: N,
    route_layer: R,
    _marker: std::marker::PhantomData<fn(U)>,
}

#[derive(Clone, Debug)]
pub struct ServiceRouter<R, S>(watch::Receiver<Shared<R, S>>);

#[derive(Debug)]
struct Shared<R, S> {
    matches: Vec<(RequestMatch, Route)>,
    routes: HashMap<Route, R>, // TODO(ver) AHashMap?
    default: S,
}

// === impl NewServiceRouter ===

impl<C, N, R: Clone> NewServiceRouter<N, R, C> {
    pub fn layer(route_layer: R) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |new_concrete| Self {
            new_concrete,
            route_layer: route_layer.clone(),
            _marker: std::marker::PhantomData,
        })
    }
}

impl<L, C, N, R, S> NewService<L> for NewServiceRouter<N, R, C>
where
    L: Param<LogicalAddr> + Param<crate::Receiver> + Clone,
    C: From<(ConcreteAddr, L)>,
    N: NewService<C> + Clone,
    N::Service: Clone,
    R: layer::Layer<N::Service>, // TODO NewDistribute<N::Service>,
    R::Service: NewService<(Route, L)>,
{
    type Service = ServiceRouter<R::Service, N::Service>;

    fn new_service(&self, target: L) -> Self::Service {
        // Spawn a background task that watches for profile updates and, when a
        // change is necessary, rebuilds stacks:
        //
        // 1. Maintain a cache of concrete backend services (i.e., load
        //    balancers). These services are shared across all routes and
        //    therefor must be cloneable (i.e., buffered).
        // 2.
        // 3. Publish these stacks so that they may be used

        // Build the initial stacks by checking the profile.
        // TODO(ver) configure a distributor.

        let mut profiles: crate::Receiver = target.param();
        let profile = profiles.borrow_and_update();

        // TODO(ver) use a different key type that is structured instead of
        // simply a name.
        let mut concretes = HashMap::with_capacity(profile.targets.len().max(1));
        for target in profile.targets.iter() {
            let addr = target.addr.clone();
            let concrete = self
                .inner
                .new_service(C::from((ConcreteAddr(addr.clone()), target.clone())));
            concretes.insert(addr.clone(), concrete);
        }
        // TODO(ver) we should make it a requirement of the provider that there
        // is always at least one backend.
        if concretes.is_empty() {
            let LogicalAddr(addr) = target.param();
            let concrete = self
                .inner
                .new_service(C::from((ConcreteAddr(addr.clone()), target.clone())));
            concretes.insert(addr, concrete);
        }

        // Build a distributor that has access to all of the concrete backends.
        let new_distribute = NewDistribute::new(concretes.iter().cloned());

        // Build the default route using the default distributor.
        let default_distribution = profile.targets;
        new_distribute.new_service(default_distribution);

        //

        let route_layer = self.route_layer.clone();
        let shared = Shared {
            matches: profile.http_routes.iter().cloned().collect(),
            routes: profile
                .http_routes
                .iter()
                .map(|(_, r)| {
                    // TODO(ver) configure a distributor.
                    route_layer
                        .layer(new_distribute.clone())
                        .new_service((r.clone(), target.clone()))
                })
                .collect(),
            default,
        };
        let (tx, rx) = watch::channel(shared);

        tokio::spawn(async move {
            let mut profiles: crate::ReceiverStream = profiles.into();
            loop {
                match profiles.next().await {
                    Some(Ok(profile)) => {
                        todo!("Publish updated stacks")
                    }
                    Some(Err(error)) => {
                        error!(%error, "Failed to receive profile");
                        return;
                    }
                    None => return,
                }
            }
        });

        ServiceRouter {
            target,
            new_route,
            default,
            rx: rx.into(),
            matches: std::sync::Arc::new([]),
            routes: HashMap::default(),
        }
    }
}

// === impl ServiceRouter ===

impl<B, T, N, R, S> Service<http::Request<B>> for ServiceRouter<T, N, R, S>
where
    T: Clone,
    N: NewService<(Route, T), Service = R> + Clone,
    R: Service<http::Request<B>, Response = S::Response, Error = Error> + Clone,
    S: Service<http::Request<B>, Error = Error>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Either<S::Future, Oneshot<R, http::Request<B>>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        // TODO(ver) This should be pending when all backends are unavailable.

        // Only process profile updates when the default route is viable. In
        // most cases is should be a common service used by all routes, so
        // testing its state is valuable.
        futures::ready!(self.default.poll_ready(cx))?;

        // If the routes have changed, update the cache.
        if let Poll::Ready(Some(Profile { http_routes, .. })) = self.rx.poll_next_unpin(cx) {
            if self.matches.len() != http_routes.len()
                || self.matches.iter().zip(&http_routes).any(|(a, b)| a != b)
            {
                debug!(routes = %http_routes.len(), "Updating HTTP routes");
                let routes = (http_routes.iter().map(|(_, r)| r.clone())).collect::<HashSet<_>>();
                self.matches = http_routes.into_iter().collect();

                // Clear out defunct routes before building any missing routes.
                self.routes.retain(|r, _| routes.contains(r));
                for route in routes.into_iter() {
                    if let hash_map::Entry::Vacant(ent) = self.routes.entry(route) {
                        let route = ent.key().clone();
                        let svc = self.new_route.new_service((route, self.target.clone()));
                        ent.insert(svc);
                    }
                }
            }
        }

        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        // If the request matches a route, use the route's service.
        if let Some(route) = super::route_for_request(&self.matches, &req) {
            if let Some(svc) = self.routes.get(route).cloned() {
                trace!(?route, "Using route service");
                return Either::Right(svc.oneshot(req));
            }

            debug_assert!(false, "Route not found in cache");
            error!(?route, "Route not found in cache. This is a bug.");
        }

        // Otherwise, use the default service.
        trace!("Using default service");
        Either::Left(self.default.call(req))
    }
}
