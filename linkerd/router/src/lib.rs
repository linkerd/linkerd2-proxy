#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use ahash::AHashMap;
use futures::{future, prelude::*};
use linkerd_error::Error;
use linkerd_stack::{layer, NewService, Oneshot, Service, ServiceExt};
use parking_lot::Mutex;
use std::{
    fmt::Debug,
    hash::Hash,
    marker::PhantomData,
    sync::Arc,
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

/// A [`NewService`] that lazy builds stacks for each `K`-typed key.
#[derive(Clone, Debug)]
pub struct NewRoute<K, L, N> {
    route_layer: L,
    inner: N,
    _marker: PhantomData<fn(K)>,
}

/// The [`Service`] constructed by [`NewRoute`].
///
/// Each request is matched against the route table and routed to the
/// appropriate
#[derive(Clone, Debug)]
pub struct Route<T, K, N, S> {
    params: T,
    new_route: N,
    routes: Arc<Mutex<AHashMap<K, S>>>,
}

// === impl NewRoute ===

impl<K, L: Clone, N> NewRoute<K, L, N> {
    pub fn new(route_layer: L, inner: N) -> Self {
        Self {
            inner,
            route_layer,
            _marker: PhantomData,
        }
    }

    pub fn layer(route_layer: L) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(route_layer.clone(), inner))
    }
}

impl<T, K, L, N, S> NewService<T> for NewRoute<K, L, N>
where
    K: Eq + Hash + Clone + Debug + Send + Sync + 'static,
    T: Clone,
    N: NewService<T>,
    L: layer::Layer<N::Service>,
    L::Service: NewService<K, Service = S>,
{
    type Service = Route<T, K, L::Service, S>;

    fn new_service(&self, target: T) -> Self::Service {
        let inner = self.inner.new_service(target.clone());
        Route {
            new_route: self.route_layer.layer(inner),
            params: target,
            routes: Default::default(),
        }
    }
}

// === impl Route ===

impl<T, N, S, Req> Service<Req> for Route<T, T::Key, N, S>
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
        match self.route(&req) {
            Ok(route) => future::Either::Left(route.oneshot(req).map_err(Into::into)),
            Err(error) => future::Either::Right({
                debug!(%error, "Failed to route request");
                future::err(error)
            }),
        }
    }
}

impl<T, K, N, S> Route<T, K, N, S> {
    #[inline]
    fn route<Req>(&self, req: &Req) -> Result<S, Error>
    where
        T: SelectRoute<Req, Key = K>,
        K: Eq + Hash + Clone + Debug + Send + Sync + 'static,
        N: NewService<K, Service = S>,
        S: Clone,
    {
        let key = self.params.select(req)?;
        let mut routes = self.routes.lock();
        let svc = routes
            .entry(key.clone())
            .or_insert_with(|| self.new_route.new_service(key.clone()));
        Ok(svc.clone())
    }
}
