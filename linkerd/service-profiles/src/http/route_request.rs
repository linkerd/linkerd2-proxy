use super::{Receiver, RequestMatch, Route};
use crate::Profile;
use futures::{future::ErrInto, prelude::*, ready, Stream};
use linkerd_error::Error;
use linkerd_stack::{layer, NewService, Param, Proxy};
use std::{
    collections::HashMap,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};
use tracing::{debug, trace};

pub fn layer<M, N: Clone, R>(
    new_route: N,
) -> impl layer::Layer<M, Service = NewRouteRequest<M, N, R>> {
    // This is saved so that the same `Arc`s are used and cloned instead of
    // calling `Route::default()` every time.
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
    rx: Pin<Box<dyn Stream<Item = Profile> + Send + Sync>>,
    inner: S,
    new_route: N,
    http_routes: Vec<(RequestMatch, Route)>,
    proxies: HashMap<Route, R>,
    default: R,
}

impl<M: Clone, N: Clone, R> Clone for NewRouteRequest<M, N, R> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            new_route: self.new_route.clone(),
            default: self.default.clone(),
            _route: self._route,
        }
    }
}

impl<T, M, N> NewService<T> for NewRouteRequest<M, N, N::Service>
where
    T: Clone + Param<Receiver>,
    M: NewService<T>,
    N: NewService<(Route, T)> + Clone,
{
    type Service = RouteRequest<T, M::Service, N, N::Service>;

    fn new_service(&mut self, target: T) -> Self::Service {
        let rx = crate::stream_profile(target.param());
        let inner = self.inner.new_service(target.clone());
        let default = self
            .new_route
            .new_service((self.default.clone(), target.clone()));
        RouteRequest {
            rx,
            target,
            inner,
            default,
            new_route: self.new_route.clone(),
            http_routes: Vec::new(),
            proxies: HashMap::new(),
        }
    }
}

impl<B, T, N, S, R> tower::Service<http::Request<B>> for RouteRequest<T, S, N, R>
where
    B: Send + 'static,
    T: Clone,
    N: NewService<(Route, T), Service = R> + Clone,
    R: Proxy<http::Request<B>, S>,
    S: tower::Service<R::Request>,
    S::Error: Into<Error>,
{
    type Response = R::Response;
    type Error = Error;
    type Future = ErrInto<R::Future, Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let mut update = None;
        while let Poll::Ready(Some(up)) = self.rx.as_mut().poll_next(cx) {
            tracing::trace!(update = ?up, "updated profile");
            update = Some(up);
        }

        // Every time the profile updates, rebuild the distribution, reusing
        // services that existed in the prior state.
        if let Some(Profile { http_routes, .. }) = update {
            debug!(routes = %http_routes.len(), "Updating HTTP routes");
            let mut proxies = HashMap::with_capacity(http_routes.len());
            for (_, ref route) in &http_routes {
                // Reuse the prior services whenever possible.
                let proxy = self.proxies.remove(&route).unwrap_or_else(|| {
                    debug!(?route, "Creating HTTP route");
                    self.new_route
                        .new_service((route.clone(), self.target.clone()))
                });
                proxies.insert(route.clone(), proxy);
            }
            self.http_routes = http_routes;
            self.proxies = proxies;
        }

        Poll::Ready(ready!(self.inner.poll_ready(cx)).map_err(Into::into))
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        for (ref condition, ref route) in &self.http_routes {
            if condition.is_match(&req) {
                trace!(?condition, "Using configured route");
                return self.proxies[route]
                    .proxy(&mut self.inner, req)
                    .err_into::<Error>();
            }
        }

        trace!("Using default route");
        self.default.proxy(&mut self.inner, req).err_into::<Error>()
    }
}
