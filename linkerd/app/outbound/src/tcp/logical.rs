use super::{Concrete, Logical, Outbound};
use crate::logical::LogicalError;
use linkerd_app_core::{
    io,
    profiles::{self, Profile},
    proxy::api_resolve::ConcreteAddr,
    svc, Error,
};
use linkerd_distribute as distribute;

#[derive(Debug, thiserror::Error)]
#[error("no route")]
pub struct NoRoute;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Params {
    logical: Logical,
    route: RouteParams,
    backends: distribute::Backends<Concrete>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct RouteParams {
    logical: Logical,
    distribution: Distribution,
}

type BackendCache<N, S> = distribute::BackendCache<Concrete, N, S>;
type Distribution = distribute::Distribution<Concrete>;

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
    pub fn push_tcp_logical<I, NSvc>(
        self,
    ) -> Outbound<
        svc::ArcNewService<
            Logical,
            impl svc::Service<I, Response = (), Error = LogicalError, Future = impl Send> + Clone,
        >,
    >
    where
        N: svc::NewService<Concrete, Service = NSvc> + Clone + Send + Sync + 'static,
        NSvc: svc::Service<I, Response = ()> + Clone + Send + Sync + 'static,
        NSvc::Future: Send,
        NSvc::Error: Into<Error>,
        I: io::AsyncRead + io::AsyncWrite + std::fmt::Debug + Send + Unpin + 'static,
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
                .push(svc::NewOneshotRoute::<Params, _, _>::layer_cached());

            // For each `Logical` target, watch its `Profile`, maintaining a
            // cache of all concrete services used by the router.
            concrete
                // Share the concrete stack with each router stack.
                .lift_new()
                // Rebuild this router stack every time the profile changes.
                .push_on_service(router)
                .push(svc::NewSpawnWatch::<Profile, _>::layer_into::<Params>())
                .push(svc::NewMapErr::layer_from_target())
                .push(svc::ArcNewService::layer())
        })
    }
}

// === impl Params ===

impl From<(Profile, Logical)> for Params {
    fn from((profile, logical): (Profile, Logical)) -> Self {
        // Create concrete targets for all of the profile's routes.
        let (backends, distribution) = if profile.targets.is_empty() {
            let concrete = super::Concrete {
                resolve: ConcreteAddr(logical.logical_addr.clone().into()),
                logical: logical.clone(),
            };
            let backends = std::iter::once(concrete.clone()).collect();
            let distribution = Distribution::first_available(std::iter::once(concrete));
            (backends, distribution)
        } else {
            let backends = profile
                .targets
                .iter()
                .map(|t| super::Concrete {
                    resolve: ConcreteAddr(t.addr.clone()),
                    logical: logical.clone(),
                })
                .collect();
            let distribution = Distribution::random_available(profile.targets.iter().cloned().map(
                |profiles::Target { addr, weight }| {
                    let concrete = Concrete {
                        logical: logical.clone(),
                        resolve: ConcreteAddr(addr),
                    };
                    (concrete, weight)
                },
            ))
            .expect("distribution must be valid");

            (backends, distribution)
        };

        let route = RouteParams {
            logical: logical.clone(),
            distribution,
        };

        Self {
            logical,
            backends,
            route,
        }
    }
}

impl svc::Param<distribute::Backends<Concrete>> for Params {
    fn param(&self) -> distribute::Backends<Concrete> {
        self.backends.clone()
    }
}

impl svc::Param<profiles::LogicalAddr> for Params {
    fn param(&self) -> profiles::LogicalAddr {
        self.logical.logical_addr.clone()
    }
}

impl<I> svc::router::SelectRoute<I> for Params {
    type Key = RouteParams;
    type Error = std::convert::Infallible;

    fn select(&self, _: &I) -> Result<Self::Key, Self::Error> {
        Ok(self.route.clone())
    }
}

// === impl RouteParams ===

impl svc::Param<Distribution> for RouteParams {
    fn param(&self) -> Distribution {
        self.distribution.clone()
    }
}
