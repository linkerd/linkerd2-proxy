use ahash::AHashMap;
use futures::{future, prelude::*};
use linkerd_error::Error;
use linkerd_stack::{layer, NewService, NewSpawnWatch, Oneshot, Param, Service, ServiceExt};
use std::{
    fmt::Debug,
    hash::Hash,
    marker::PhantomData,
    sync::Arc,
    task::{Context, Poll},
};
use tracing::{debug, error};

#[derive(Debug, Clone)]
pub struct RouteKeys<K>(Arc<[K]>)
where
    K: Eq + Hash + Clone;

pub trait SelectRoute<Req> {
    type Key: Eq + Hash + Clone + Debug + Send + Sync + 'static;
    type Error: std::error::Error + Send + Sync + 'static;

    /// Given a a request, returns the key matching this request.
    ///
    /// If no route matches the request, this method returns an error.
    fn select<'r>(&self, req: &'r Req) -> Result<&Self::Key, Self::Error>;
}

pub type NewRouteWatch<T, K, L, N> = NewSpawnWatch<T, NewRoute<K, L, N>>;

/// A [`NewService`] that produces [`Route`]s.
///
/// This is to be called by [`SpawnWatch`] with a clone of the watched value.
///
/// [`SpawnWatch`]: linkerd_app_core::svc::SpawnWatch
#[derive(Clone, Debug)]
pub struct NewRoute<K, L, N> {
    route_layer: L,
    new_backends: N,
    _marker: PhantomData<fn(K)>,
}

/// The [`Service`] constructed by [`NewRoute`].
///
/// Each request is matched against the route table and routed to the
/// appropriate
#[derive(Clone, Debug)]
pub struct Route<T, R, S> {
    router: R,
    routes: Arc<AHashMap<T, S>>,
}

#[derive(Debug, thiserror::Error)]
#[error("unknown route: {0:?}")]
pub struct UnknownRoute<K: std::fmt::Debug>(K);

// === impl NewRoute ===

impl<K, L: Clone, N> NewRoute<K, L, N> {
    pub fn watch<T>(route_layer: L, new_backends: N) -> NewSpawnWatch<T, Self> {
        NewSpawnWatch::new(Self {
            new_backends,
            route_layer,
            _marker: PhantomData,
        })
    }

    pub fn layer<T>(
        route_layer: L,
    ) -> impl layer::Layer<N, Service = NewSpawnWatch<T, Self>> + Clone {
        layer::mk(move |inner| Self::watch(route_layer.clone(), inner))
    }
}

impl<T, K, L, N, S> NewService<T> for NewRoute<K, L, N>
where
    K: Eq + Hash + Clone + Debug + Send + Sync + 'static,
    T: Param<RouteKeys<K>> + Clone,
    N: NewService<T>,
    L: layer::Layer<N::Service>,
    L::Service: NewService<K, Service = S>,
{
    type Service = Route<K, T, S>;

    fn new_service(&self, target: T) -> Self::Service {
        let backends = self.new_backends.new_service(target.clone());
        let new_route = self.route_layer.layer(backends);

        let RouteKeys(keys) = target.param();
        let routes = keys
            .iter()
            .map(|key| (key.clone(), new_route.new_service(key.clone())))
            .collect();

        Route {
            router: target,
            routes: Arc::new(routes),
        }
    }
}

// === impl Route ===

impl<R, S, Req> Service<Req> for Route<R::Key, R, S>
where
    R: SelectRoute<Req>,
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
        match self.route(&req).cloned() {
            Ok(route) => future::Either::Left(route.oneshot(req).map_err(Into::into)),
            Err(error) => future::Either::Right({
                debug!(%error, "Failed to route request");
                future::err(error)
            }),
        }
    }
}

impl<T, R, S> Route<T, R, S> {
    #[inline]
    fn route<Req>(&self, req: &Req) -> Result<&S, Error>
    where
        T: Eq + Hash + Clone + Debug + Send + Sync + 'static,
        R: SelectRoute<Req, Key = T>,
        S: Clone,
    {
        let key = self.router.select(req)?;
        self.routes
            .get(key)
            .ok_or_else(|| UnknownRoute(key.clone()).into())
    }
}

impl<T, R: Default, S> Default for Route<T, R, S> {
    fn default() -> Self {
        Self {
            router: Default::default(),
            routes: Default::default(),
        }
    }
}

// === impl RouteKeys ===

impl<K> FromIterator<K> for RouteKeys<K>
where
    K: Eq + Hash + Clone + Debug + Send + Sync + 'static,
{
    fn from_iter<T: IntoIterator<Item = K>>(iter: T) -> Self {
        Self(Arc::from(iter.into_iter().collect::<Vec<_>>()))
    }
}
