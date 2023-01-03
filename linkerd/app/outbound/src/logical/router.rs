use std::fmt::Debug;

use linkerd_app_core::{profiles, profiles::Profile, svc, NameAddr};
use linkerd_distribute as distribute;
use linkerd_router as router;
use tokio::sync::watch;

pub use linkerd_router::{RouteKeys, SelectRoute};

#[derive(Clone, Debug)]
pub struct Target<T> {
    pub logical: NameAddr,
    // TODO(ver) we want this to be a type that can _produce_ a `Params<T>`...
    pub rx: watch::Receiver<Params<T>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Params<T> {
    pub logical: NameAddr,
    pub backends: Backends,
    pub routes: T,
}

// TODO(ver) this will probably need a protocol-specific dimension as well...
// TODO(ver) change the address type
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BackendParams {
    pub logical: NameAddr,
    pub concrete: NameAddr,
}

pub type Backends = distribute::Backends<BackendParams>;
pub type Distribution = distribute::Distribution<BackendParams>;
pub type CacheNewDistribute<N, S> = distribute::CacheNewDistribute<BackendParams, N, S>;
pub type Distribute<S> = distribute::Distribute<BackendParams, S>;

pub type NewRouteDistribute<T, K, L, N, S> =
    router::NewRouteWatch<Params<T>, K, L, CacheNewDistribute<N, S>>;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RouteParams<T> {
    pub logical: NameAddr,
    pub distribution: Distribution,
    pub params: T,
}

pub fn watch<T, K, L, N, S>(route_layer: L, new_backends: N) -> NewRouteDistribute<T, K, L, N, S>
where
    L: Clone,
    NewRouteDistribute<T, K, L, N, S>: svc::NewService<Target<T>>,
{
    // TODO(ver) we probably want to put a layer between the watch and the
    // NewRoute that modifies the target type.
    router::NewRoute::watch(route_layer, CacheNewDistribute::new(new_backends))
}

pub fn layer<T, K, L, N, S>(
    route_layer: L,
) -> impl svc::layer::Layer<N, Service = NewRouteDistribute<T, K, L, N, S>> + Clone
where
    L: Clone,
    NewRouteDistribute<T, K, L, N, S>: svc::NewService<Target<T>>,
{
    svc::layer::mk(move |inner| watch(route_layer.clone(), inner))
}

impl<T> svc::Param<profiles::LogicalAddr> for Target<T> {
    fn param(&self) -> profiles::LogicalAddr {
        profiles::LogicalAddr(self.logical.clone())
    }
}

impl<T> svc::Param<watch::Receiver<Params<T>>> for Target<T> {
    fn param(&self) -> watch::Receiver<Params<T>> {
        self.rx.clone()
    }
}

impl<T> Params<T> {
    pub fn new(logical: NameAddr, profile: &Profile, routes: T) -> Self {
        let backends = profile
            .target_addrs
            .iter()
            .map(|concrete| BackendParams {
                logical: logical.clone(),
                concrete: concrete.clone(),
            })
            .collect();

        Self {
            logical,
            backends,
            routes,
        }
    }
}

impl<T> svc::Param<profiles::LogicalAddr> for Params<T> {
    fn param(&self) -> profiles::LogicalAddr {
        profiles::LogicalAddr(self.logical.clone())
    }
}

impl<T> svc::Param<Backends> for Params<T> {
    fn param(&self) -> Backends {
        self.backends.clone()
    }
}

impl<T> svc::Param<profiles::LogicalAddr> for RouteParams<T> {
    fn param(&self) -> profiles::LogicalAddr {
        profiles::LogicalAddr(self.logical.clone())
    }
}

impl<T> svc::Param<Distribution> for RouteParams<T> {
    fn param(&self) -> Distribution {
        self.distribution.clone()
    }
}
