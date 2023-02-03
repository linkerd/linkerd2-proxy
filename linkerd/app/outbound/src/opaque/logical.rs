use super::Dispatch;
use crate::Outbound;
use linkerd_app_core::{
    io,
    profiles::{self, Profile},
    proxy::tcp::balance,
    svc,
    transport::addrs::*,
    Error, Infallible,
};
use linkerd_distribute as distribute;
use std::{fmt::Debug, hash::Hash, time};
use tokio::sync::watch;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Concrete<T> {
    dispatch: Dispatch,
    parent: T,
}

#[derive(Debug, thiserror::Error)]
#[error("no route")]
pub struct NoRoute;

#[derive(Debug, thiserror::Error)]
#[error("logical service {addr}: {source}")]
pub struct LogicalError {
    addr: profiles::LogicalAddr,
    #[source]
    source: Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Params<T: Eq + Hash + Clone + Debug> {
    parent: T,
    route: RouteParams<T>,
    backends: distribute::Backends<Concrete<T>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct RouteParams<T> {
    parent: T,
    distribution: Distribution<T>,
}

type BackendCache<T, N, S> = distribute::BackendCache<Concrete<T>, N, S>;
type Distribution<T> = distribute::Distribution<Concrete<T>>;

#[derive(Clone, Debug)]
struct Routable<T> {
    parent: T,
    addr: profiles::LogicalAddr,
    profile: profiles::Receiver,
}

// === impl Outbound ===

impl<N> Outbound<N> {
    /// Builds a `NewService` that produces a router service for each logical
    /// target.
    ///
    /// The router uses discovery information (provided on the target) to
    /// support per-request connection routing over a set of concrete inner
    /// services. Only available inner services are used for routing. When
    /// there are no available backends, requests are failed with a
    /// [`svc::stack::LoadShedError`].
    ///
    // TODO(ver) make the outer target type generic/parameterized.
    pub fn push_opaque_logical<T, I, NSvc>(
        self,
    ) -> Outbound<
        svc::ArcNewService<
            T,
            impl svc::Service<I, Response = (), Error = Error, Future = impl Send> + Clone,
        >,
    >
    where
        T: svc::Param<Remote<ServerAddr>>,
        T: svc::Param<Option<profiles::Receiver>>,
        T: Eq + Hash + Clone + Debug + Send + Sync + 'static,
        N: svc::NewService<Concrete<T>, Service = NSvc> + Clone + Send + Sync + 'static,
        NSvc: svc::Service<I, Response = ()> + Clone + Send + Sync + 'static,
        NSvc::Future: Send,
        NSvc::Error: Into<Error>,
        I: io::AsyncRead + io::AsyncWrite + Debug + Send + Unpin + 'static,
    {
        self.map_stack(|_, _, concrete| {
            let route = svc::layers()
                // The router does not take the backend's availability into
                // consideration, so we must eagerly fail requests to prevent
                // leaking tasks onto the runtime.
                .push_on_service(svc::LoadShed::layer());

            // A `NewService`--instantiated once per logical target--that caches
            // a set of concrete services so that, as the watch provides new
            // `Params`, we can reuse inner services.
            let router = svc::layers()
                // Each `RouteParams` provides a `Distribution` that is used to
                // choose a concrete service for a given route.
                .push(BackendCache::layer())
                // Lazily cache a service for each `RouteParams`
                // returned from the `SelectRoute` impl.
                .push_on_service(route)
                .push(svc::NewOneshotRoute::<Params<T>, _, _>::layer_cached());

            // For each `Logical` target, watch its `Profile`, maintaining a
            // cache of all concrete services used by the router.
            concrete
                .clone()
                // Share the concrete stack with each router stack.
                .lift_new()
                // Rebuild this router stack every time the profile changes.
                .push_on_service(router)
                .check_new_new_service::<Routable<T>, Params<T>, I>()
                .push(svc::NewSpawnWatch::<Profile, _>::layer_into::<Params<T>>())
                .push(svc::NewMapErr::layer_from_target::<LogicalError, _>())
                .check_new::<Routable<T>>()
                .push_switch(
                    |parent: T| -> Result<_, Infallible> {
                        if let Some(profile) = parent.param() {
                            if let Some(addr) = profile.logical_addr() {
                                return Ok(svc::Either::A(Routable {
                                    addr,
                                    parent,
                                    profile,
                                }));
                            }

                            if let Some((addr, meta)) = profile.endpoint() {
                                return Ok(svc::Either::B(Concrete {
                                    dispatch: Dispatch::Forward(addr, meta),
                                    parent,
                                }));
                            }
                        }

                        let Remote(ServerAddr(addr)) = parent.param();
                        Ok(svc::Either::B(Concrete {
                            dispatch: Dispatch::Forward(addr, Default::default()),
                            parent,
                        }))
                    },
                    concrete.check_new::<Concrete<T>>().into_inner(),
                )
                .check_new::<T>()
                .push(svc::ArcNewService::layer())
        })
    }
}

// === impl Routable ===

impl<T> svc::Param<watch::Receiver<profiles::Profile>> for Routable<T> {
    fn param(&self) -> watch::Receiver<profiles::Profile> {
        self.profile.clone().into()
    }
}

// === impl Params ===

impl<T> From<(Profile, Routable<T>)> for Params<T>
where
    T: Eq + Hash + Clone + Debug,
{
    fn from((profile, routable): (Profile, Routable<T>)) -> Self {
        const EWMA: balance::EwmaConfig = balance::EwmaConfig {
            default_rtt: time::Duration::from_millis(30),
            decay: time::Duration::from_secs(10),
        };

        // Create concrete targets for all of the profile's routes.
        let (backends, distribution) = if profile.targets.is_empty() {
            let profiles::LogicalAddr(addr) = routable.addr;
            let concrete = Concrete {
                dispatch: Dispatch::Balance(addr, EWMA),
                parent: routable.parent.clone(),
            };
            let backends = std::iter::once(concrete.clone()).collect();
            let distribution = Distribution::first_available(std::iter::once(concrete));
            (backends, distribution)
        } else {
            let backends = profile
                .targets
                .iter()
                .map(|t| Concrete {
                    dispatch: Dispatch::Balance(t.addr.clone(), EWMA),
                    parent: routable.parent.clone(),
                })
                .collect();
            let distribution = Distribution::random_available(profile.targets.iter().cloned().map(
                |profiles::Target { addr, weight }| {
                    let concrete = Concrete {
                        dispatch: Dispatch::Balance(addr, EWMA),
                        parent: routable.parent.clone(),
                    };
                    (concrete, weight)
                },
            ))
            .expect("distribution must be valid");

            (backends, distribution)
        };

        let route = RouteParams {
            parent: routable.parent.clone(),
            distribution,
        };

        Self {
            parent: routable.parent,
            backends,
            route,
        }
    }
}

impl<T> svc::Param<distribute::Backends<Concrete<T>>> for Params<T>
where
    T: Clone + Eq + Hash + Debug,
{
    fn param(&self) -> distribute::Backends<Concrete<T>> {
        self.backends.clone()
    }
}

impl<T> svc::Param<profiles::LogicalAddr> for Params<T>
where
    T: svc::Param<profiles::LogicalAddr>,
    T: Clone + Eq + Hash + Debug,
{
    fn param(&self) -> profiles::LogicalAddr {
        self.parent.param()
    }
}

impl<T, I> svc::router::SelectRoute<I> for Params<T>
where
    T: Clone + Eq + Hash + Debug,
{
    type Key = RouteParams<T>;
    type Error = std::convert::Infallible;

    fn select(&self, _: &I) -> Result<Self::Key, Self::Error> {
        Ok(self.route.clone())
    }
}

// === impl RouteParams ===

impl<T: Clone> svc::Param<Distribution<T>> for RouteParams<T> {
    fn param(&self) -> Distribution<T> {
        self.distribution.clone()
    }
}

// === impl LogicalError ===

impl<T> From<(&Routable<T>, Error)> for LogicalError {
    fn from((target, source): (&Routable<T>, Error)) -> Self {
        Self {
            addr: target.addr.clone(),
            source,
        }
    }
}

// === impl Concrete ===

impl<T> svc::Param<Option<profiles::LogicalAddr>> for Concrete<T>
where
    T: svc::Param<Option<profiles::LogicalAddr>>,
{
    fn param(&self) -> Option<profiles::LogicalAddr> {
        self.parent.param()
    }
}
impl<T> svc::Param<Dispatch> for Concrete<T> {
    fn param(&self) -> Dispatch {
        self.dispatch.clone()
    }
}
