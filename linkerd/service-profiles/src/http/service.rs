use super::{RequestMatch, Route};
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
    http_routes: Vec<(RequestMatch, Route)>,
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
    N: NewService<(Option<Route>, T)> + Clone,
{
    type Service = ServiceRouter<T, N, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let rx = target.param();
        let default = self.0.new_service((None, target.clone()));
        ServiceRouter {
            default,
            target,
            rx: rx.into(),
            http_routes: Vec::new(),
            new_route: self.0.clone(),
        }
    }
}

// === impl ServiceRouter ===

impl<B, T, N, S> Service<http::Request<B>> for ServiceRouter<T, N, S>
where
    T: Clone,
    T: super::RequestTarget,
    N: NewService<(Option<Route>, T), Service = S> + Clone,
    S: Service<http::Request<B>> + Clone,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Oneshot<S, http::Request<B>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        // If the routes have been updated, update the cache.
        if let Poll::Ready(Some(Profile { http_routes, .. })) = self.rx.poll_next_unpin(cx) {
            debug!(routes = %http_routes.len(), "Updating HTTP routes");
            self.http_routes = http_routes;
        }

        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        let inner = match super::route_for_request(&self.http_routes, &req) {
            Some(route) => {
                // If the request matches a route, use the route's service.
                trace!(?route, "Using route service");
                let target = self.target.request_target(route, &req);
                self.new_route.new_service((Some(route.clone()), target))
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
