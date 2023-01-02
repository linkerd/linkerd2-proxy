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
pub struct RouteKeys<K>(Arc<[K]>);

pub trait SelectRoute<Req> {
    type Key: Eq + Hash + Clone + Debug + Send + Sync + 'static;
    type Error: std::error::Error + Send + Sync + 'static;

    /// Given a a request, returns the key matching this request.
    ///
    /// If no route matches the request, this method returns an error.
    fn select<'r>(&self, req: &'r Req) -> Result<&Self::Key, Self::Error>;
}

/// A [`NewService`] that produces [`NewRoute`]s.
///
/// This is to be called by [`NewSpawnWatch`] with the original target type
/// (i.e. that provides a [`tokio::sync::watch::Receiver`] param).
#[derive(Clone, Debug)]
pub struct NewRouteWatch<K, N, L> {
    new_backends: N,
    route_layer: L,
    _marker: PhantomData<fn(K)>,
}

/// A [`NewService`] that produces [`Route`]s.
///
/// This is to be called by [`SpawnWatch`] with a clone of the watched value.
///
/// [`SpawnWatch`]: linkerd_app_core::svc::SpawnWatch
#[derive(Clone, Debug)]
pub struct NewRoute<T, K, N, L> {
    target: T,
    new_backends: N,
    route_layer: L,
    _marker: PhantomData<fn(K)>,
}

/// The [`Service`] constructed by [`NewRoute`].
///
/// Each request is matched against the route table and routed to the
/// appropriate
#[derive(Clone, Debug)]
pub struct Route<R, K, S> {
    router: R,
    routes: Arc<AHashMap<K, S>>,
}

#[derive(Debug, thiserror::Error)]
#[error("unknown route: {0:?}")]
pub struct UnknownRoute<K: std::fmt::Debug>(K);

// === impl NewRouteWatch ===

impl<K, N, L: Clone> NewRouteWatch<K, N, L> {
    pub fn layer<T>(
        route_layer: L,
    ) -> impl layer::Layer<N, Service = NewSpawnWatch<T, Self>> + Clone {
        layer::mk(move |new_backends| {
            NewSpawnWatch::new(Self {
                new_backends,
                route_layer: route_layer.clone(),
                _marker: PhantomData,
            })
        })
    }
}

impl<T, K, N, L> NewService<T> for NewRouteWatch<K, N, L>
where
    T: Clone,
    N: NewService<T>,
    L: Clone,
{
    type Service = NewRoute<T, K, N::Service, L>;

    fn new_service(&self, target: T) -> Self::Service {
        let new_backends = self.new_backends.new_service(target.clone());
        NewRoute {
            target,
            new_backends,
            route_layer: self.route_layer.clone(),
            _marker: self._marker,
        }
    }
}

// === impl NewRoute ===

impl<P, K, T, N, L, S> NewService<P> for NewRoute<T, K, N, L>
where
    P: Param<RouteKeys<K>> + Clone,
    K: Eq + Hash + Clone,
    T: Clone,
    N: NewService<P>,
    L: layer::Layer<N::Service>,
    L::Service: NewService<(K, T), Service = S>,
{
    type Service = Route<P, K, S>;

    fn new_service(&self, params: P) -> Self::Service {
        let RouteKeys(keys) = params.param();

        let backends = self.new_backends.new_service(params.clone());
        let new_route = self.route_layer.layer(backends);

        Route {
            router: params,
            routes: Arc::new(
                keys.iter()
                    .map(|key| {
                        let svc = new_route.new_service((key.clone(), self.target.clone()));
                        (key.clone(), svc)
                    })
                    .collect(),
            ),
        }
    }
}

// === impl Route ===

impl<R, S, Req> Service<Req> for Route<R, R::Key, S>
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
        match self.route(&req) {
            Ok(svc) => future::Either::Left(svc.oneshot(req).map_err(Into::into)),
            Err(error) => future::Either::Right({
                debug!(%error, "Failed to route request");
                future::err(error)
            }),
        }
    }
}

impl<R, K, S> Route<R, K, S> {
    #[inline]
    fn route<Req>(&self, req: &Req) -> Result<S, Error>
    where
        R: SelectRoute<Req, Key = K>,
        K: Eq + Hash + Clone + Debug + Send + Sync + 'static,
        S: Clone,
    {
        let key = self.router.select(req)?;
        self.routes
            .get(key)
            .cloned()
            .ok_or_else(|| UnknownRoute(key.clone()).into())
    }
}

impl<R: Default, K, S> Default for Route<R, K, S> {
    fn default() -> Self {
        Self {
            router: R::default(),
            routes: Arc::new(AHashMap::default()),
        }
    }
}
