use super::Outbound;
use linkerd_app_core::{
    io,
    profiles::{self, Profile},
    proxy::{api_resolve::ConcreteAddr, tcp::balance},
    svc, Error, NameAddr,
};
use linkerd_distribute as distribute;
use std::{fmt::Debug, hash::Hash, time};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Concrete<T> {
    addr: NameAddr,
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
    pub fn push_tcp_logical<T, I, NSvc>(
        self,
    ) -> Outbound<
        svc::ArcNewService<
            T,
            impl svc::Service<I, Response = (), Error = LogicalError, Future = impl Send> + Clone,
        >,
    >
    where
        T: svc::Param<tokio::sync::watch::Receiver<profiles::Profile>>,
        T: svc::Param<profiles::LogicalAddr>,
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
                // Share the concrete stack with each router stack.
                .lift_new()
                // Rebuild this router stack every time the profile changes.
                .push_on_service(router)
                .check_new_new_service::<T, Params<T>, I>()
                .push(svc::NewSpawnWatch::<Profile, _>::layer_into::<Params<T>>())
                .check_new::<T>()
                .push(svc::NewMapErr::layer_from_target())
                .push(svc::ArcNewService::layer())
        })
    }
}

// === impl Params ===

impl<T> From<(Profile, T)> for Params<T>
where
    T: svc::Param<profiles::LogicalAddr>,
    T: Eq + Hash + Clone + Debug,
{
    fn from((profile, parent): (Profile, T)) -> Self {
        // Create concrete targets for all of the profile's routes.
        let (backends, distribution) = if profile.targets.is_empty() {
            let profiles::LogicalAddr(addr) = parent.param();
            let concrete = Concrete {
                addr,
                parent: parent.clone(),
            };
            let backends = std::iter::once(concrete.clone()).collect();
            let distribution = Distribution::first_available(std::iter::once(concrete));
            (backends, distribution)
        } else {
            let backends = profile
                .targets
                .iter()
                .map(|t| Concrete {
                    addr: t.addr.clone(),
                    parent: parent.clone(),
                })
                .collect();
            let distribution = Distribution::random_available(profile.targets.iter().cloned().map(
                |profiles::Target { addr, weight }| {
                    let concrete = Concrete {
                        parent: parent.clone(),
                        addr,
                    };
                    (concrete, weight)
                },
            ))
            .expect("distribution must be valid");

            (backends, distribution)
        };

        let route = RouteParams {
            parent: parent.clone(),
            distribution,
        };

        Self {
            parent,
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

impl<T> From<(&T, Error)> for LogicalError
where
    T: svc::Param<profiles::LogicalAddr>,
{
    fn from((target, source): (&T, Error)) -> Self {
        Self {
            addr: target.param(),
            source,
        }
    }
}

// === impl Concrete ===

impl<T> svc::Param<balance::EwmaConfig> for Concrete<T> {
    fn param(&self) -> balance::EwmaConfig {
        balance::EwmaConfig {
            default_rtt: time::Duration::from_millis(30),
            decay: time::Duration::from_secs(10),
        }
    }
}

impl<T> svc::Param<ConcreteAddr> for Concrete<T> {
    fn param(&self) -> ConcreteAddr {
        ConcreteAddr(self.addr.clone())
    }
}

impl<T> svc::Param<Option<profiles::LogicalAddr>> for Concrete<T>
where
    T: svc::Param<Option<profiles::LogicalAddr>>,
{
    fn param(&self) -> Option<profiles::LogicalAddr> {
        self.parent.param()
    }
}
