use super::Outbound;
use linkerd_app_core::{io, svc, Error};
use linkerd_distribute as distribute;
use linkerd_proxy_client_policy::{self as policy, ClientPolicy};
use std::{fmt::Debug, hash::Hash};
use tokio::sync::watch;

#[derive(Clone, Debug, thiserror::Error)]
#[error("opaque connection has no routes")]
pub struct NoRoute(());

#[derive(Clone, Debug, PartialEq, Eq)]
struct Params {
    policy: ClientPolicy,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct RouteParams {
    policy: policy::opaque::Policy,
}

type BackendCache<N, S> = distribute::BackendCache<policy::Backend, N, S>;
type Distribution = distribute::Distribution<policy::Backend>;

// === impl Params ===

impl<T> From<(ClientPolicy, T)> for Params {
    fn from((policy, _): (ClientPolicy, T)) -> Self {
        Params { policy }
    }
}

impl svc::Param<distribute::Backends<policy::Backend>> for Params {
    fn param(&self) -> distribute::Backends<policy::Backend> {
        distribute::Backends::from_iter(self.policy.backends.iter().cloned())
    }
}

impl<I> svc::router::SelectRoute<I> for Params {
    type Key = RouteParams;
    type Error = NoRoute;

    fn select(&self, _: &I) -> Result<Self::Key, Self::Error> {
        let policy = match self.policy.protocol {
            policy::Protocol::Detect { opaque, .. } | policy::Protocol::Opaque(opaque) => {
                opaque.policy.clone().ok_or(NoRoute(()))?
            }
            _ => return Err(NoRoute(())),
        };
        Ok(RouteParams { policy })
    }
}

// === impl RouteParams ===

impl svc::Param<Distribution> for RouteParams {
    fn param(&self) -> Distribution {
        match self.policy.distribution {
            policy::RouteDistribution::Empty => Distribution::Empty,
            policy::RouteDistribution::FirstAvailable(backends) => {
                Distribution::first_available(backends.iter().cloned().map(|rb| rb.backend))
            }
            policy::RouteDistribution::RandomAvailable(backends) => Distribution::random_available(
                backends
                    .iter()
                    .cloned()
                    .map(|(rb, weight)| (rb.backend, weight)),
            )
            .expect("distribution must be valid"),
        }
    }
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
    pub fn push_tcp_logical<T, I, NSvc>(
        self,
    ) -> Outbound<
        svc::ArcNewService<
            T,
            impl svc::Service<I, Response = (), Error = Error, Future = impl Send> + Clone,
        >,
    >
    where
        T: svc::Param<watch::Receiver<ClientPolicy>>,
        // T: svc::Param<Option<discover::LogicalProfile>>,
        // T: Eq + Clone + Debug + Hash + Send + Sync + 'static,
        T: Clone + Send + Sync + 'static,
        N: svc::NewService<policy::Backend, Service = NSvc> + Clone + Send + Sync + 'static,
        NSvc: svc::Service<I, Response = (), Error = Error> + Clone + Send + Sync + 'static,
        NSvc::Future: Send,
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
                .check_new_service::<policy::Backend, I>()
                // Share the concrete stack with each router stack.
                .push_new_clone()
                .check_new_new_service::<Params, policy::Backend, I>()
                // Rebuild this router stack every time the profile changes.
                .push_on_service(router)
                .check_new_new_service::<T, Params, I>()
                .push(svc::NewSpawnWatch::<ClientPolicy, _>::layer_into::<Params>())
                .check_new::<T>()
                .check_new_service::<T, I>()
                .push(svc::ArcNewService::layer())
        })
    }
}

/*
impl<T> From<(Profile, T)> for Params {
    fn from((profile, inner): (Profile, T)) -> Self {
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
                        inner: inner.clone(),
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
*/
