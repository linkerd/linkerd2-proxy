use super::{retry, CanonicalDstHeader, Logical};
use crate::Outbound;
use linkerd_app_core::{
    classify, metrics,
    profiles::{self, Profile},
    proxy::api_resolve::ConcreteAddr,
    proxy::http,
    svc, Error, NameAddr,
};
use linkerd_distribute as distribute;
use std::{fmt::Debug, hash::Hash, sync::Arc};

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
struct Params<T: Clone + Debug + Eq + Hash> {
    parent: T,
    routes: Arc<[(profiles::http::RequestMatch, RouteParams<T>)]>,
    backends: distribute::Backends<Concrete<T>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct RouteParams<T> {
    parent: T,
    profile: profiles::http::Route,
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
    /// support per-request routing over a set of concrete inner services.
    /// Only available inner services are used for routing. When there are no
    /// available backends, requests are failed with a [`svc::stack::LoadShedError`].
    ///
    // TODO(ver) make the outer target type generic/parameterized.
    pub fn push_http_logical<NSvc>(
        self,
    ) -> Outbound<
        svc::ArcNewService<
            Logical,
            impl svc::Service<
                    http::Request<http::BoxBody>,
                    Response = http::Response<http::BoxBody>,
                    Error = LogicalError,
                    Future = impl Send,
                > + Clone,
        >,
    >
    where
        N: svc::NewService<Concrete<Logical>, Service = NSvc> + Clone + Send + Sync + 'static,
        NSvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>
            + Clone
            + Send
            + Sync
            + 'static,
        NSvc::Error: Into<Error>,
        NSvc::Future: Send,
    {
        self.map_stack(|_, rt, concrete| {
            let route = svc::layers()
                .push_on_service(
                    svc::layers()
                        .push(http::BoxRequest::layer())
                        // The router does not take the backend's availability into
                        // consideration, so we must eagerly fail requests to prevent
                        // leaking tasks onto the runtime.
                        .push(svc::LoadShed::layer()),
                )
                .push(http::insert::NewInsert::<RouteParams<Logical>, _>::layer())
                .push(
                    rt.metrics
                        .proxy
                        .http_profile_route_actual
                        .to_layer::<classify::Response, _, RouteParams<Logical>>(),
                )
                // Depending on whether or not the request can be
                // retried, it may have one of two `Body` types. This
                // layer unifies any `Body` type into `BoxBody`.
                .push_on_service(http::BoxRequest::erased())
                // Sets an optional retry policy.
                .push(retry::layer(
                    rt.metrics.proxy.http_profile_route_retry.clone(),
                ))
                // Sets an optional request timeout.
                .push(http::NewTimeout::layer())
                // Records per-route metrics.
                .push(
                    rt.metrics
                        .proxy
                        .http_profile_route
                        .to_layer::<classify::Response, _, RouteParams<Logical>>(),
                )
                // Sets the per-route response classifier as a request
                // extension.
                .push(classify::NewClassify::layer())
                // TODO(ver) .push(svc::NewMapErr::layer_from_target::<RouteError, _>())
                .push_on_service(http::BoxResponse::layer());

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
                .push(svc::NewOneshotRoute::<Params<Logical>, _, _>::layer_cached());

            // For each `Logical` target, watch its `Profile`, rebuilding a
            // router stack.
            concrete
                // Share the concrete stack with each router stack.
                .lift_new()
                // Rebuild this router stack every time the profile changes.
                .push_on_service(router)
                .push(svc::NewSpawnWatch::<Profile, _>::layer_into::<
                    Params<Logical>,
                >())
                // Add l5d-dst-canonical header to requests.
                //
                // TODO(ver) move this into the endpoint stack so that we can only
                // set this on meshed connections.
                //
                // TODO(ver) do we need to strip headers here?
                .push(http::NewHeaderFromTarget::<CanonicalDstHeader, _>::layer())
                .push(svc::NewMapErr::layer_from_target())
                .push(svc::ArcNewService::layer())
        })
    }
}

// === impl Params ===

impl From<(Profile, Logical)> for Params<Logical> {
    fn from((profile, logical): (Profile, Logical)) -> Self {
        // Create concrete targets for all of the profile's routes.
        let (backends, distribution) = if profile.targets.is_empty() {
            let concrete = Concrete {
                addr: logical.logical_addr.clone().into(),
                parent: logical.clone(),
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
                    parent: logical.clone(),
                })
                .collect();
            let distribution = Distribution::random_available(profile.targets.iter().cloned().map(
                |profiles::Target { addr, weight }| {
                    let concrete = Concrete {
                        addr,
                        parent: logical.clone(),
                    };
                    (concrete, weight)
                },
            ))
            .expect("distribution must be valid");

            (backends, distribution)
        };

        let routes = profile
            .http_routes
            .iter()
            .cloned()
            .map(|(req_match, profile)| {
                let params = RouteParams {
                    profile,
                    parent: logical.clone(),
                    distribution: distribution.clone(),
                };
                (req_match, params)
            })
            // Add a default route.
            .chain(std::iter::once((
                profiles::http::RequestMatch::default(),
                RouteParams {
                    profile: Default::default(),
                    parent: logical.clone(),
                    distribution: distribution.clone(),
                },
            )))
            .collect::<Arc<[(_, _)]>>();

        Self {
            parent: logical,
            backends,
            routes,
        }
    }
}

impl svc::Param<distribute::Backends<Concrete<Logical>>> for Params<Logical> {
    fn param(&self) -> distribute::Backends<Concrete<Logical>> {
        self.backends.clone()
    }
}

impl svc::Param<profiles::LogicalAddr> for Params<Logical> {
    fn param(&self) -> profiles::LogicalAddr {
        self.parent.logical_addr.clone()
    }
}

impl<B> svc::router::SelectRoute<http::Request<B>> for Params<Logical> {
    type Key = RouteParams<Logical>;
    type Error = NoRoute;

    fn select(&self, req: &http::Request<B>) -> Result<Self::Key, Self::Error> {
        profiles::http::route_for_request(&*self.routes, req)
            .ok_or(NoRoute)
            .cloned()
    }
}

// === impl RouteParams ===

impl svc::Param<profiles::LogicalAddr> for RouteParams<Logical> {
    fn param(&self) -> profiles::LogicalAddr {
        self.parent.logical_addr.clone()
    }
}

impl svc::Param<Distribution<Logical>> for RouteParams<Logical> {
    fn param(&self) -> Distribution<Logical> {
        self.distribution.clone()
    }
}

impl svc::Param<profiles::http::Route> for RouteParams<Logical> {
    fn param(&self) -> profiles::http::Route {
        self.profile.clone()
    }
}

impl svc::Param<metrics::ProfileRouteLabels> for RouteParams<Logical> {
    fn param(&self) -> metrics::ProfileRouteLabels {
        metrics::ProfileRouteLabels::outbound(self.parent.logical_addr.clone(), &self.profile)
    }
}

impl svc::Param<http::ResponseTimeout> for RouteParams<Logical> {
    fn param(&self) -> http::ResponseTimeout {
        http::ResponseTimeout(self.profile.timeout())
    }
}

impl classify::CanClassify for RouteParams<Logical> {
    type Classify = classify::Request;

    fn classify(&self) -> classify::Request {
        self.profile.response_classes().clone().into()
    }
}

// === impl LogicalError ===

impl<T: svc::Param<profiles::LogicalAddr>> From<(&T, Error)> for LogicalError {
    fn from((target, source): (&T, Error)) -> Self {
        Self {
            addr: target.param(),
            source,
        }
    }
}

// === impl Concrete ===

impl svc::Param<http::Version> for Concrete<Logical> {
    fn param(&self) -> http::Version {
        self.parent.param()
    }
}

impl svc::Param<ConcreteAddr> for Concrete<Logical> {
    fn param(&self) -> ConcreteAddr {
        ConcreteAddr(self.addr.clone())
    }
}

impl svc::Param<Option<profiles::LogicalAddr>> for Concrete<Logical> {
    fn param(&self) -> Option<profiles::LogicalAddr> {
        Some(self.parent.logical_addr.clone())
    }
}

impl svc::Param<http::balance::EwmaConfig> for Concrete<Logical> {
    fn param(&self) -> http::balance::EwmaConfig {
        http::balance::EwmaConfig {
            default_rtt: std::time::Duration::from_millis(30),
            decay: std::time::Duration::from_secs(10),
        }
    }
}
