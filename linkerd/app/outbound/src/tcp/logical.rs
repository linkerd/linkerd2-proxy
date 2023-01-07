use super::{Concrete, Logical, Outbound};
use linkerd_app_core::{
    io,
    profiles::{self, Profile},
    proxy::api_resolve::ConcreteAddr,
    svc, Error,
};
use linkerd_distribute as distribute;
use tracing::debug_span;

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
    pub fn push_tcp_logical<I, NSvc>(
        self,
    ) -> Outbound<
        svc::ArcNewService<
            Logical,
            impl svc::Service<I, Response = (), Error = Error, Future = impl Send> + Clone,
        >,
    >
    where
        N: svc::NewService<Concrete, Service = NSvc> + Clone + Send + Sync + 'static,
        NSvc: svc::Service<I, Response = (), Error = Error> + Clone + Send + Sync + 'static,
        NSvc::Future: Send,
        I: io::AsyncRead + io::AsyncWrite + std::fmt::Debug + Send + Unpin + 'static,
    {
        self.map_stack(|config, _, concrete| {
            // A `NewService`--instantiated once per logical target--that caches
            // a set of concrete services so that, as the watch provides new
            // `Params`, we can reuse inner services.
            let router = svc::layers()
                // Each `RouteParams` provides a `Distribution` that is used to
                // choose a concrete service for a given route.
                .push(BackendCache::layer())
                // Lazily cache a service for each `RouteParams`
                // returned from the `SelectRoute` impl.
                .push(svc::NewOneshotRoute::<Params, _, _>::layer_cached());

            // For each `Logical` target, watch its `Profile`, maintaining a
            // cache of all concrete services used by the router.
            let watch = concrete
                // Share the concrete stack each router stack.
                .push_new_clone()
                .check_new_new::<Logical, Concrete>()
                // Rebuild this router stack every time the profile changes.
                .push_on_service(router)
                .check_new_new::<Logical, Params>()
                .push(svc::NewSpawnWatch::<Profile, _>::layer_into::<Params>());

            // This caches each logical stack so that it can be reused across
            // per-connection server stacks (i.e., created by the
            // DetectService).
            //
            // TODO(ver) Update the detection stack so this dynamic caching can
            // be removed.
            watch
                .check_new_service::<Logical, I>()
                .push_idle_cache(config.discovery_idle_timeout)
                .instrument(|_: &Logical| debug_span!("tcp"))
                .check_new_service::<Logical, I>()
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
