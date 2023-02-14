use super::concrete;
use crate::Outbound;
use linkerd_app_core::{
    io,
    profiles::{self, Profile},
    proxy::{api_resolve::Metadata, tcp::balance},
    svc,
    transport::addrs::*,
    Error, Infallible, NameAddr,
};
use linkerd_distribute as distribute;
use std::{fmt::Debug, hash::Hash, time};
use tokio::sync::watch;

#[cfg(test)]
mod tests;

#[derive(Clone, Debug)]
pub enum Target {
    Route(NameAddr, profiles::Receiver),
    Forward(Remote<ServerAddr>, Metadata),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Concrete<T> {
    target: concrete::Target,
    parent: T,
}

#[derive(Debug, thiserror::Error)]
#[error("no route")]
pub struct NoRoute;

#[derive(Debug, thiserror::Error)]
#[error("logical service {addr}: {source}")]
pub struct LogicalError {
    addr: NameAddr,
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
    addr: NameAddr,
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
    pub fn push_opaq_logical<T, I, NSvc>(
        self,
    ) -> Outbound<
        svc::ArcNewService<
            T,
            impl svc::Service<I, Response = (), Error = Error, Future = impl Send> + Clone,
        >,
    >
    where
        // Opaque logical target.
        T: svc::Param<Target>,
        T: Eq + Hash + Clone + Debug + Send + Sync + 'static,
        // Server-side socket.
        I: io::AsyncRead + io::AsyncWrite + Debug + Send + Unpin + 'static,
        // Concrete stack.
        N: svc::NewService<Concrete<T>, Service = NSvc> + Clone + Send + Sync + 'static,
        NSvc: svc::Service<I, Response = ()> + Clone + Send + Sync + 'static,
        NSvc::Future: Send,
        NSvc::Error: Into<Error>,
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
                .push(svc::NewSpawnWatch::<Profile, _>::layer_into::<Params<T>>())
                .push(svc::NewMapErr::layer_from_target::<LogicalError, _>())
                .push_switch(
                    |parent: T| -> Result<_, Infallible> {
                        Ok(match parent.param() {
                            Target::Route(addr, profile) => svc::Either::A(Routable {
                                addr,
                                parent,
                                profile,
                            }),
                            Target::Forward(addr, meta) => svc::Either::B(Concrete {
                                target: concrete::Target::Forward(addr, meta),
                                parent,
                            }),
                        })
                    },
                    concrete.into_inner(),
                )
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
            let concrete = Concrete {
                target: concrete::Target::Balance(routable.addr, EWMA),
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
                    target: concrete::Target::Balance(t.addr.clone(), EWMA),
                    parent: routable.parent.clone(),
                })
                .collect();
            let distribution = Distribution::random_available(profile.targets.iter().cloned().map(
                |profiles::Target { addr, weight }| {
                    let concrete = Concrete {
                        target: concrete::Target::Balance(addr, EWMA),
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

impl<T> std::ops::Deref for Concrete<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.parent
    }
}

impl<T> svc::Param<Option<profiles::LogicalAddr>> for Concrete<T>
where
    T: svc::Param<Option<profiles::Receiver>>,
{
    fn param(&self) -> Option<profiles::LogicalAddr> {
        (**self).param()?.logical_addr()
    }
}

impl<T> svc::Param<concrete::Target> for Concrete<T> {
    fn param(&self) -> concrete::Target {
        self.target.clone()
    }
}

// === impl Target ===

impl std::cmp::PartialEq for Target {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Route(laddr, _), Self::Route(raddr, _)) => laddr == raddr,
            (Self::Forward(laddr, lmeta), Self::Forward(raddr, rmeta)) => {
                laddr == raddr && lmeta == rmeta
            }
            _ => false,
        }
    }
}

impl std::cmp::Eq for Target {}

impl std::hash::Hash for Target {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match self {
            Self::Route(addr, _) => {
                addr.hash(state);
            }
            Self::Forward(addr, meta) => {
                addr.hash(state);
                meta.hash(state);
            }
        }
    }
}
