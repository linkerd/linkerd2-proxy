use super::{RequestMatch, Route};
use crate::{Profile, Receiver, ReceiverStream};
use futures::{future, prelude::*};
use linkerd_error::{Error, Result};
use linkerd_stack::{layer, NewService, Param, Proxy, Service};
use std::{
    collections::{hash_map, HashMap, HashSet},
    task::{Context, Poll},
};
use tracing::{debug, trace};

/// A router that uses a per-route `Proxy` to wrap a common underlying
/// `Service`.
///
/// This router is similar to `linkerd_stack::NewRouter` and
/// `linkerd_cache::Cache` with a few differences:
///
/// * It's `Proxy`-specific;
/// * Routes are constructed eagerly as the profile updates;
/// * Routes are removed eagerly as the profile updates (i.e. there's no
///   idle-oriented eviction).
#[derive(Clone, Debug)]
pub struct NewProxyRouter<M, N> {
    new_proxy: M,
    new_service: N,
}

#[derive(Debug)]
pub struct ProxyRouter<T, N, S> {
    new_proxy: N,
    inner: S,
    target: T,
    rx: ReceiverStream,
    http_routes: Vec<(RequestMatch, Route)>,
}

// === impl NewProxyRouter ===

impl<M: Clone, N> NewProxyRouter<M, N> {
    pub fn layer(new_proxy: M) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |new_service| Self {
            new_service,
            new_proxy: new_proxy.clone(),
        })
    }
}

impl<T, M, N> NewService<T> for NewProxyRouter<M, N>
where
    T: Param<Receiver> + Clone,
    N: NewService<T> + Clone,
    M: NewService<(Route, T)> + Clone,
{
    type Service = ProxyRouter<T, M, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let rx = target.param();
        let inner = self.new_service.new_service(target.clone());
        ProxyRouter {
            inner,
            target,
            rx: rx.into(),
            http_routes: Vec::new(),
            new_proxy: self.new_proxy.clone(),
        }
    }
}

// === impl ProxyRouter ===

type ProxyResponseFuture<F1, E1, F2, E2> =
    future::Either<future::MapErr<F1, fn(E1) -> Error>, future::MapErr<F2, fn(E2) -> Error>>;

impl<B, T, N, S, Rsp> Service<http::Request<B>> for ProxyRouter<T, N, S>
where
    T: Clone + super::RequestTarget,
    N: NewService<(Route, T)> + Clone,
    N::Service: Proxy<http::Request<B>, S, Request = http::Request<B>, Response = Rsp>,
    S: Service<http::Request<B>, Response = Rsp>,
    S::Error: Into<Error>,
{
    type Response = Rsp;
    type Error = Error;
    type Future = ProxyResponseFuture<
        S::Future,
        S::Error,
        <N::Service as Proxy<http::Request<B>, S>>::Future,
        <N::Service as Proxy<http::Request<B>, S>>::Error,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<()>> {
        // Poll the inner service first so we don't bother updating routes unless we can actually
        // use them.
        futures::ready!(self.inner.poll_ready(cx).map_err(Into::into))?;

        // If the routes have been updated, update the cache.
        if let Poll::Ready(Some(Profile { http_routes, .. })) = self.rx.poll_next_unpin(cx) {
            debug!(routes = %http_routes.len(), "Updating HTTP routes");
            self.http_routes = http_routes;
        }

        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        match super::route_for_request(&self.http_routes, &req) {
            None => future::Either::Left({
                // Use the inner service directly if no route matches the
                // request.
                trace!("No routes matched");
                self.inner.call(req).map_err(Into::into)
            }),
            Some(route) => future::Either::Right({
                // Otherwise, wrap the inner service with the route-specific
                // proxy.
                trace!(?route, "Using route proxy");
                let target = self.target.request_target(route, &req);
                self.new_proxy
                    .new_service((route.clone(), target))
                    .proxy(&mut self.inner, req)
                    .map_err(Into::into)
            }),
        }
    }
}
