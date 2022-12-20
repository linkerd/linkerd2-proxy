use super::{RequestMatch, Route};
use crate::{Profile, Receiver, ReceiverStream};
use futures::{future::Either, prelude::*};
use linkerd_error::Error;
use linkerd_stack::{layer, NewService, Oneshot, Param, Service, ServiceExt};
use std::{
    collections::{hash_map, HashMap, HashSet},
    task::{Context, Poll},
};
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
#[derive(Debug)]
pub struct NewServiceRouter<U, N, R> {
    new_inner: N,
    route_layer: R,
    _marker: std::marker::PhantomData<fn(U)>,
}

#[derive(Clone, Debug)]
pub struct ServiceRouter<T, N, R, S> {
    target: T,
    new_route: N,

    rx: ReceiverStream,
    matches: std::sync::Arc<[(RequestMatch, Route)]>,
    routes: HashMap<Route, R>,

    default: S,
}

// === impl NewServiceRouter ===

impl<U, N, R: Clone> NewServiceRouter<U, N, R> {
    pub fn layer(route_layer: R) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |new_inner| Self {
            new_inner,
            route_layer: route_layer.clone(),
            _marker: std::marker::PhantomData,
        })
    }
}

impl<U, N: Clone, R: Clone> Clone for NewServiceRouter<U, N, R> {
    fn clone(&self) -> Self {
        Self {
            new_inner: self.new_inner.clone(),
            route_layer: self.route_layer.clone(),
            _marker: self._marker,
        }
    }
}

impl<T, U, N, NSvc, R, RSvc> NewService<T> for NewServiceRouter<U, N, R>
where
    T: Param<Receiver> + Clone,
    U: From<T>,
    N: NewService<T>,
    N::Service: NewService<U, Service = NSvc>,
    R: layer::Layer<N::Service>,
    R::Service: NewService<(Route, T), Service = RSvc>,
{
    type Service = ServiceRouter<T, R::Service, RSvc, NSvc>;

    fn new_service(&self, target: T) -> Self::Service {
        let rx: Receiver = target.param();
        let new_inner = self.new_inner.new_service(target.clone());
        let default = new_inner.new_service(target.clone().into());
        let new_route = self.route_layer.layer(new_inner);
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
