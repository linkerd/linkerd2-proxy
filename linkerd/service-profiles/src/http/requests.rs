use super::{Receiver, RequestMatch, Route};
use crate::Profile;
use futures::prelude::*;
use linkerd2_stack::{layer, NewService, Proxy};
use std::{
    collections::HashMap,
    future::Future,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};
use tracing::debug;

pub fn layer<M, N: Clone, R>(
    new_route: N,
) -> impl layer::Layer<M, Service = NewRouteRequest<M, N, R>> {
    let default = Route::default();
    layer::mk(move |inner| NewRouteRequest {
        inner,
        new_route: new_route.clone(),
        default: default.clone(),
        _route: PhantomData,
    })
}

pub struct NewRouteRequest<M, N, R> {
    inner: M,
    new_route: N,
    default: Route,
    _route: PhantomData<R>,
}

pub struct RouteRequest<T, S, N, R> {
    target: T,
    rx: Receiver,
    inner: S,
    new_route: N,
    http_routes: Vec<(RequestMatch, Route)>,
    proxies: HashMap<Route, R>,
    default: R,
}

impl<T, M, N> tower::Service<(Receiver, T)> for NewRouteRequest<M, N, N::Service>
where
    T: Clone + Send + 'static,
    M: tower::Service<(Receiver, T)>,
    M::Future: Send + 'static,
    N: NewService<(Route, T)> + Clone + Send + 'static,
{
    type Response = RouteRequest<T, M::Response, N, N::Service>;
    type Error = M::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, M::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, (rx, target): (Receiver, T)) -> Self::Future {
        let new_route = self.new_route.clone();
        let default_route = self.default.clone();
        Box::pin(
            self.inner
                .call((rx.clone(), target.clone()))
                .map_ok(move |inner| {
                    let default = new_route.new_service((default_route, target.clone()));
                    RouteRequest {
                        rx,
                        target,
                        inner,
                        default,
                        new_route,
                        http_routes: Vec::new(),
                        proxies: HashMap::new(),
                    }
                }),
        )
    }
}

impl<B, T, N, S, R> tower::Service<http::Request<B>> for RouteRequest<T, S, N, R>
where
    B: Send + 'static,
    T: Clone,
    N: NewService<(Route, T), Service = R> + Clone,
    R: Proxy<http::Request<B>, S>,
    S: tower::Service<R::Request>,
{
    type Response = R::Response;
    type Error = R::Error;
    type Future = R::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let mut update = None;
        while let Poll::Ready(Some(up)) = self.rx.poll_recv_ref(cx) {
            update = Some(up.clone());
        }
        // Every time the profile updates, rebuild the distribution, reusing
        // services that existed in the prior state.
        if let Some(Profile { http_routes, .. }) = update {
            let mut proxies = HashMap::with_capacity(http_routes.len());
            for (_, ref route) in &http_routes {
                // Reuse the prior services whenever possible.
                let proxy = self.proxies.remove(&route).unwrap_or_else(|| {
                    self.new_route
                        .new_service((route.clone(), self.target.clone()))
                });
                proxies.insert(route.clone(), proxy);
            }
            self.http_routes = http_routes;
            self.proxies = proxies;
        }

        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        for (ref condition, ref route) in &self.http_routes {
            if condition.is_match(&req) {
                debug!(?condition, "Using configured route");
                return self.proxies[route].proxy(&mut self.inner, req);
            }
        }

        debug!("Using default route");
        self.default.proxy(&mut self.inner, req)
    }
}
