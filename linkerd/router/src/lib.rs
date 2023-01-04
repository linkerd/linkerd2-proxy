// TODO(ver) Replace `stack::NewRouter` with this.

#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use futures::{future, prelude::*};
use linkerd_error::Error;
use linkerd_stack::{layer, NewCache, NewService, Oneshot, Service, ServiceExt};
use std::{
    fmt::Debug,
    hash::Hash,
    task::{Context, Poll},
};
use tracing::debug;

pub trait SelectRoute<Req> {
    type Key: Eq + Hash + Clone + Debug + Send + Sync + 'static;
    type Error: std::error::Error + Send + Sync + 'static;

    /// Given a a request, returns the key matching this request.
    ///
    /// If no route matches the request, this method returns an error.
    fn select<'r>(&self, req: &'r Req) -> Result<&Self::Key, Self::Error>;
}

/// A [`NewService`] that builds `Route` services for targets that implement
/// [`SelectRoute`].
#[derive(Clone, Debug)]
pub struct NewRoute<L, N> {
    route_layer: L,
    inner: N,
}

/// Dispatches requests to a new `S`-typed inner service.
///
/// Each request is matched against the route table and routed to a new inner
/// service.
#[derive(Clone, Debug)]
pub struct Route<T, N> {
    params: T,
    new_route: N,
}

// === impl NewRoute ===

impl<L: Clone, N> NewRoute<L, N> {
    pub fn new(route_layer: L, inner: N) -> Self {
        Self { inner, route_layer }
    }

    pub fn layer(route_layer: L) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(route_layer.clone(), inner))
    }

    /// Returns a new `NewRoute` layer that retains & reuses its inner services.
    pub fn layer_cached<K>(
        route_layer: L,
    ) -> impl layer::Layer<N, Service = NewRoute<L, NewCache<K, N>>> + Clone {
        layer::mk(move |inner: N| NewRoute::new(route_layer.clone(), NewCache::new(inner)))
    }
}

impl<T, L, N> NewService<T> for NewRoute<L, N>
where
    T: Clone,
    N: NewService<T>,
    L: layer::Layer<N::Service>,
{
    type Service = Route<T, L::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let inner = self.inner.new_service(target.clone());
        Route {
            new_route: self.route_layer.layer(inner),
            params: target,
        }
    }
}

// === impl Route ===

impl<T, N, S, Req> Service<Req> for Route<T, N>
where
    T: SelectRoute<Req>,
    N: NewService<T::Key, Service = S>,
    S: Service<Req> + Clone,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::Either<
        future::MapErr<Oneshot<S, Req>, fn(S::Error) -> Error>,
        future::Ready<Result<S::Response, Error>>,
    >;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        // TODO(ver) figure out how backpressure should work here. Ideally, we
        // should only advertise readiness when at least one backend is ready.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Req) -> Self::Future {
        match self.params.select(&req) {
            Ok(key) => future::Either::Left({
                let route = self.new_route.new_service(key.clone());
                route.oneshot(req).map_err(Into::into)
            }),
            Err(e) => future::Either::Right({
                let error = e.into();
                debug!(%error, "Failed to route request");
                future::err(error)
            }),
        }
    }
}
