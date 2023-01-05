use super::{RequestMatch, Route};
use crate::{Profile, Receiver, ReceiverStream};
use futures::prelude::*;
use linkerd_stack::{layer, NewService, Oneshot, Param, Service, ServiceExt};
use std::{
    collections::{hash_map, HashMap, HashSet},
    sync::Arc,
    task::{Context, Poll},
};
use tracing::{debug, trace};

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
pub struct NewServiceRouter<N>(N);

#[derive(Debug)]
pub struct ServiceRouter<T, N, S> {
    target: T,
    new_route: N,
    routes: Routes<S>,
    default: S,
    profile: Receiver,
    profile_rx: ReceiverStream,
}

#[derive(Clone, Debug)]
struct Routes<S> {
    matches: Arc<[(RequestMatch, Route)]>,
    services: HashMap<Route, S>,
}

impl<S> Default for Routes<S> {
    fn default() -> Self {
        Self {
            matches: Profile::default().http_routes,
            services: HashMap::new(),
        }
    }
}

impl<T: Clone, N: Clone, S: Clone> Clone for ServiceRouter<T, N, S> {
    fn clone(&self) -> Self {
        Self {
            target: self.target.clone(),
            new_route: self.new_route.clone(),
            default: self.default.clone(),
            routes: self.routes.clone(),
            profile: self.profile.clone(),
            profile_rx: self.profile.clone().into(),
        }
    }
}

// === impl NewServiceRouter ===

impl<N> NewServiceRouter<N> {
    pub fn layer() -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(Self)
    }
}

impl<T, N> NewService<T> for NewServiceRouter<N>
where
    T: Param<Receiver> + Clone,
    N: NewService<(Option<Route>, T)> + Clone,
{
    type Service = ServiceRouter<T, N, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let rx = target.param();
        let default = self.0.new_service((None, target.clone()));
        ServiceRouter {
            target,
            new_route: self.0.clone(),
            routes: Default::default(),
            default,
            profile: rx.clone(),
            profile_rx: rx.into(),
        }
    }
}

// === impl ServiceRouter ===

impl<B, T, N, S> Service<http::Request<B>> for ServiceRouter<T, N, S>
where
    T: Clone,
    N: NewService<(Option<Route>, T), Service = S> + Clone,
    S: Service<http::Request<B>> + Clone,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Oneshot<S, http::Request<B>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        // If the routes have been updated, update the cache.
        if let Poll::Ready(Some(Profile { http_routes, .. })) = self.profile_rx.poll_next_unpin(cx)
        {
            debug!(routes = %http_routes.len(), "Updating HTTP routes");
            let routes = http_routes
                .iter()
                .map(|(_, r)| r.clone())
                .collect::<HashSet<_>>();
            self.routes.matches = http_routes;

            // Clear out defunct routes before building any missing routes.
            self.routes.services.retain(|r, _| routes.contains(r));
            for route in routes.into_iter() {
                if let hash_map::Entry::Vacant(ent) = self.routes.services.entry(route) {
                    let route = ent.key().clone();
                    let svc = self
                        .new_route
                        .new_service((Some(route), self.target.clone()));
                    ent.insert(svc);
                }
            }
        }

        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        let inner = match super::route_for_request(&self.routes.matches, &req) {
            Some(route) => {
                // If the request matches a route, use the route's service.
                trace!(?route, "Using route service");
                self.routes
                    .services
                    .get(route)
                    .expect("route must exist")
                    .clone()
            }
            None => {
                // Otherwise, use the default service.
                trace!("No routes matched");
                self.default.clone()
            }
        };

        inner.oneshot(req)
    }
}
