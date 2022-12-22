use super::{RequestMatch, Route};
use crate::LogicalAddr;
use futures::{future::Either, prelude::*};
use linkerd_addr::NameAddr;
use linkerd_proxy_api_resolve::ConcreteAddr;
use linkerd_stack::{layer, NewCloneService, NewService, Oneshot, Param, Service, ServiceExt};
use std::{
    collections::HashMap,
    task::{Context, Poll},
};
use tokio::sync::watch;
use tracing::{error, trace};

type Distribution = crate::Distribution<NameAddr>;
type NewDistribute<S> = crate::NewDistribute<NameAddr, S>;
type Distribute<S> = crate::Distribute<NameAddr, S>;

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
    new_backend: N,
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
        layer::mk(move |new_backend| Self {
            new_backend,
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
    N::Service: Clone + Send + Sync + 'static,
    R: layer::Layer<NewCloneService<Distribute<N::Service>>>, // TODO NewDistribute<N::Service>,
    R::Service: NewService<(Route, L), Service = S>,
    S: Send + Sync + 'static,
{
    type Service = ServiceRouter<S, Distribute<N::Service>>;

    fn new_service(&self, target: L) -> Self::Service {
        // Spawn a background task that watches for profile updates and, when a
        // change is necessary, rebuilds stacks:
        //
        // 1. Maintain a cache of backend backend services (i.e., load
        //    balancers). These services are shared across all routes and
        //    therefor must be cloneable (i.e., buffered).
        // 2.
        // 3. Publish these stacks so that they may be used

        let mut profiles: crate::Receiver = target.param();
        let (tx, rx) = {
            // Build the initial stacks by checking the profile.
            let profile = profiles.borrow_and_update();

            // TODO(ver) use a different key type that is structured instead of
            // simply a name.
            let mut backends = HashMap::with_capacity(profile.targets.len().max(1));
            for t in profile.targets.iter() {
                let addr = t.addr.clone();
                let backend = self
                    .new_backend
                    .new_service(C::from((ConcreteAddr(addr.clone()), target.clone())));
                backends.insert(addr.clone(), backend);
            }
            // TODO(ver) we should make it a requirement of the provider that there
            // is always at least one backend.
            if backends.is_empty() {
                let LogicalAddr(addr) = target.param();
                let backend = self
                    .new_backend
                    .new_service(C::from((ConcreteAddr(addr.clone()), target.clone())));
                backends.insert(addr, backend);
            }

            // Create a stack that can distribute requests to the backend backends.
            let new_distribute = NewDistribute::new(
                backends
                    .iter()
                    .map(|(addr, svc)| (addr.clone(), svc.clone())),
            );

            // Build a single distribution service that will be shared across all routes.
            //
            // TODO(ver) Move this into the route stack so that each route's
            // distribution may vary.
            let distribution = profile
                .targets
                .iter()
                .cloned()
                .map(|crate::Target { addr, weight }| (addr, weight))
                .collect::<Distribution>();
            let distribute = new_distribute.new_service(distribution);

            let new_route = self
                .route_layer
                .layer(NewCloneService::from(distribute.clone()));
            let routes = profile
                .http_routes
                .iter()
                .map(|(_, r)| {
                    let svc = new_route.new_service((r.clone(), target.clone()));
                    (r.clone(), svc)
                })
                .collect();

            watch::channel(Shared {
                default: distribute,
                matches: profile.http_routes.clone(),
                routes,
            })
        };

        tokio::spawn(async move {
            let mut profiles = crate::ReceiverStream::from(profiles);
            while let Some(_profile) = profiles.next().await {
                // Update `backends`.
                // New distribution.
                // New routes.
                // Publish new shared state.
                todo!();
            }
            drop(tx);
        });

        ServiceRouter(rx)
    }
}

// === impl ServiceRouter ===

impl<B, R, S> Service<http::Request<B>> for ServiceRouter<R, S>
where
    R: Service<http::Request<B>, Response = S::Response, Error = S::Error> + Clone,
    S: Service<http::Request<B>> + Clone,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Either<Oneshot<S, http::Request<B>>, Oneshot<R, http::Request<B>>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        // TODO(ver) figure out how backpressure should work here. Ideally, we
        // should only advertise readiness when at least one backend is ready.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        let Shared {
            matches,
            routes,
            default,
        } = &*self.0.borrow();

        // If the request matches a route, use the route's service.
        if let Some(route) = super::route_for_request(matches, &req) {
            if let Some(svc) = routes.get(route).cloned() {
                trace!(?route, "Using route service");
                return Either::Right(svc.oneshot(req));
            }

            debug_assert!(false, "Route not found in cache");
            error!(?route, "Route not found in cache. This is a bug.");
        }

        // Otherwise, use the default service.
        trace!("Using default service");
        Either::Left(default.clone().oneshot(req))
    }
}
