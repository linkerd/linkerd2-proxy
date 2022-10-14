use super::{Route, RoutePolicy};
use crate::{Profile, Receiver, ReceiverStream};
use futures::prelude::*;
use linkerd_stack::{layer, NewService, Oneshot, Param, Service, ServiceExt};
use std::{
    collections::{hash_map, HashMap, HashSet},
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
    new_route: N,
    target: T,
    rx: ReceiverStream,
    http_routes: Option<Route>,
    services: HashMap<RoutePolicy, S>,
    default: S,
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
    N: NewService<(Option<RoutePolicy>, T)> + Clone,
{
    type Service = ServiceRouter<T, N, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let rx = target.param();
        let default = self.0.new_service((None, target.clone()));
        ServiceRouter {
            default,
            target,
            rx: rx.into(),
            http_routes: None,
            services: HashMap::new(),
            new_route: self.0.clone(),
        }
    }
}

// === impl ServiceRouter ===

impl<B, T, N, S> Service<http::Request<B>> for ServiceRouter<T, N, S>
where
    T: Clone,
    N: NewService<(Option<RoutePolicy>, T), Service = S> + Clone,
    S: Service<http::Request<B>> + Clone,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Oneshot<S, http::Request<B>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        // If the routes have been updated, update the cache.
        if let Poll::Ready(Some(Profile { http_routes, .. })) = self.rx.poll_next_unpin(cx) {
            debug!(
                routes = http_routes
                    .as_ref()
                    .map(|routes| routes.rules.len())
                    .unwrap_or(0),
                "Updating HTTP routes"
            );

            if let Some(route) = http_routes {
                let route_policies = route
                    .rules
                    .iter()
                    .map(|rule| rule.policy.clone())
                    .collect::<HashSet<_>>();
                // Clear out defunct routes before building any missing routes.
                self.services.retain(|r, _| route_policies.contains(r));
                for route in route_policies.into_iter() {
                    if let hash_map::Entry::Vacant(ent) = self.services.entry(route) {
                        let route = ent.key().clone();
                        let svc = self
                            .new_route
                            .new_service((Some(route), self.target.clone()));
                        ent.insert(svc);
                    }
                }
                self.http_routes = Some(route);
            } else {
                self.http_routes = None;
            }
        }

        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        let matched = self
            .http_routes
            .as_ref()
            .and_then(|route| route.match_request(&req));
        let inner = match matched {
            Some((_, route)) => {
                // If the request matches a route, use the route's service.
                trace!(?route, "Using route service");
                self.services.get(route).expect("route must exist").clone()
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
