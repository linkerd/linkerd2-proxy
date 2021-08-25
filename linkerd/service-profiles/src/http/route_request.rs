use super::{RequestMatch, Route};
use crate::{Profile, Receiver, ReceiverStream};
use futures::{prelude::*, ready};
use linkerd_error::Error;
use linkerd_http_box::BoxBody;
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
    layer::mk(move |inner| NewRouteRequest {
        inner,
        new_route: new_route.clone(),
        _route: PhantomData,
    })
}

pub struct NewRouteRequest<M, N, R> {
    inner: M,
    new_route: N,
    _route: PhantomData<R>,
}

pub struct RouteRequest<T, S, N, R> {
    target: T,
    rx: ReceiverStream,
    inner: S,
    new_route: N,
    http_routes: Vec<(RequestMatch, Route)>,
    proxies: HashMap<Route, R>,
}

impl<M: Clone, N: Clone, R> Clone for NewRouteRequest<M, N, R> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            new_route: self.new_route.clone(),
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
        let rx = target.param();
        let inner = self.inner.new_service(target.clone());
        RouteRequest {
            rx: rx.into(),
            target,
            inner,
            new_route: self.new_route.clone(),
            http_routes: Vec::new(),
            proxies: HashMap::new(),
        }
    }
}

impl<T, N, S, R> tower::Service<http::Request<BoxBody>> for RouteRequest<T, S, N, R>
where
    T: Clone,
    N: NewService<(Route, T), Service = R> + Clone,
    R: Proxy<
        http::Request<BoxBody>,
        S,
        Request = http::Request<BoxBody>,
        Response = http::Response<BoxBody>,
    >,
    R::Future: Send + 'static,
    S: tower::Service<http::Request<BoxBody>, Response = http::Response<BoxBody>>,
    S::Future: Send + 'static,
    S::Error: Into<Error>,
{
    type Response = http::Response<BoxBody>;
    type Error = Error;
    type Future = Pin<
        Box<
            dyn std::future::Future<Output = Result<http::Response<BoxBody>, Error>>
                + Send
                + 'static,
        >,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let mut update = None;
        while let Poll::Ready(Some(up)) = self.rx.poll_next_unpin(cx) {
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
                let proxy = self.proxies.remove(route).unwrap_or_else(|| {
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

    fn call(&mut self, req: http::Request<BoxBody>) -> Self::Future {
        for (ref condition, ref route) in &self.http_routes {
            if condition.is_match(&req) {
                trace!(?condition, "Using configured route");
                return Box::pin(
                    self.proxies[route]
                        .proxy(&mut self.inner, req)
                        .err_into::<Error>(),
                );
            }
        }

        trace!("No routes matched");
        Box::pin(self.inner.call(req).err_into::<Error>())
    }
}
