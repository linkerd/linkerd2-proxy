use super::{retry, CanonicalDstHeader, Concrete, Logical};
use crate::Outbound;
use linkerd_app_core::{
    classify, metrics,
    profiles::{self, Profile},
    proxy::api_resolve::ConcreteAddr,
    proxy::http,
    svc, Error,
};
use linkerd_distribute as distribute;
use linkerd_router as router;
use std::sync::Arc;
use tracing::debug_span;

#[derive(Debug, thiserror::Error)]
#[error("no route")]
pub struct NoRoute;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Params {
    logical: Logical,
    routes: Arc<[(profiles::http::RequestMatch, RouteParams)]>,
    backends: distribute::Backends<Concrete>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct RouteParams {
    logical: Logical,
    profile: profiles::http::Route,
    distribution: Distribution,
}

type CacheNewDistribute<N, S> = distribute::CacheNewDistribute<Concrete, N, S>;
type Distribution = distribute::Distribution<Concrete>;

// === impl Outbound ===

impl<N> Outbound<N> {
    // TODO(ver) make the outer target type generic/parameterized.
    pub fn push_http_logical<NSvc>(
        self,
    ) -> Outbound<
        svc::ArcNewService<
            Logical,
            impl svc::Service<
                    http::Request<http::BoxBody>,
                    Response = http::Response<http::BoxBody>,
                    Error = Error,
                    Future = impl Send,
                > + Clone,
        >,
    >
    where
        N: svc::NewService<Concrete, Service = NSvc> + Clone + Send + Sync + 'static,
        NSvc: svc::Service<
                http::Request<http::BoxBody>,
                Response = http::Response<http::BoxBody>,
                Error = Error,
            > + Clone
            + Send
            + Sync
            + 'static,
        NSvc::Future: Send,
    {
        self.map_stack(|config, rt, concrete| {
            let route = svc::layers()
                .push_on_service(http::BoxRequest::layer())
                .push(http::insert::NewInsert::<RouteParams, _>::layer())
                .push(
                    rt.metrics
                        .proxy
                        .http_profile_route_actual
                        .to_layer::<classify::Response, _, RouteParams>(),
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
                        .to_layer::<classify::Response, _, RouteParams>(),
                )
                // Sets the per-route response classifier as a request
                // extension.
                .push(classify::NewClassify::layer())
                .push_on_service(http::BoxResponse::layer());

            // A `NewService`--instantiated once per logical target--that caches
            // a set of concrete services so that, as the watch provides new
            // `Params`, we can reuse inner services.
            let router = svc::layers()
                // Each `RouteParams` provides a `Distribution` that is used to
                // choose a concrete service for a given route.
                .push(CacheNewDistribute::layer())
                // Lazily cache a service for each `RouteParams`
                // returned from the `SelectRoute` impl.
                .push_on_service(route)
                .push(router::NewRoute::layer_cached());

            // For each `Logical` target, watch its `Profile`, rebuilding a
            // router stack.
            let watch = concrete
                .check_new_service::<Concrete, http::Request<http::BoxBody>>()
                // Share the concrete stack each router stack.
                .push_new_clone()
                .check_new_new::<Logical, Concrete>()
                // Rebuild this router stack every time the profile changes.
                .push_on_service(router)
                .check_new_new::<Logical, Params>()
                .push(svc::NewSpawnWatch::<Profile, _>::layer_into::<Params>());

            // Caches each logical stack so that it can be reused across
            // per-connection HTTP server stacks (i.e. created by the
            // `DetectService`).
            //
            // TODO(ver) Update the detection stack so this dynamic caching can
            // be removed.
            //
            // XXX(ver) This cache key includes the HTTP version. Should it?
            watch
                .check_new_service::<Logical, http::Request<http::BoxBody>>()
                // Strips headers that may be set by this proxy and add an
                // outbound canonical-dst-header.
                .push(http::NewHeaderFromTarget::<CanonicalDstHeader, _>::layer())
                .push_idle_cache(config.discovery_idle_timeout)
                .instrument(|l: &Logical| debug_span!("logical", service = %l.logical_addr))
                .push(svc::ArcNewService::layer())
        })
    }
}

// === impl Params ===

impl From<(Profile, Logical)> for Params {
    fn from((profile, logical): (Profile, Logical)) -> Self {
        // Create concrete targets for all of the profile's routes.
        let backends = if profile.targets.is_empty() {
            std::iter::once(super::Concrete {
                resolve: ConcreteAddr(logical.logical_addr.clone().into()),
                logical: logical.clone(),
            })
            .collect()
        } else {
            profile
                .targets
                .iter()
                .map(|t| super::Concrete {
                    resolve: ConcreteAddr(t.addr.clone()),
                    logical: logical.clone(),
                })
                .collect()
        };

        let distribution = Distribution::random_available(profile.targets.iter().cloned().map(
            |profiles::Target { addr, weight }| {
                let concrete = super::Concrete {
                    logical: logical.clone(),
                    resolve: ConcreteAddr(addr),
                };
                (concrete, weight)
            },
        ))
        .expect("distribution must be valid");

        let routes = profile
            .http_routes
            .iter()
            .cloned()
            .map(|(req_match, profile)| {
                let params = RouteParams {
                    profile,
                    logical: logical.clone(),
                    distribution: distribution.clone(),
                };
                (req_match, params)
            })
            // Add a default route.
            .chain(std::iter::once((
                profiles::http::RequestMatch::default(),
                RouteParams {
                    profile: Default::default(),
                    logical: logical.clone(),
                    distribution: distribution.clone(),
                },
            )))
            .collect::<Arc<[(_, _)]>>();

        Self {
            logical,
            backends,
            routes,
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

impl<B> router::SelectRoute<http::Request<B>> for Params {
    type Key = RouteParams;
    type Error = NoRoute;

    fn select(&self, req: &http::Request<B>) -> Result<Self::Key, Self::Error> {
        profiles::http::route_for_request(&*self.routes, req)
            .ok_or(NoRoute)
            .cloned()
    }
}

// === impl RouteParams ===

impl svc::Param<profiles::LogicalAddr> for RouteParams {
    fn param(&self) -> profiles::LogicalAddr {
        self.logical.logical_addr.clone()
    }
}

impl svc::Param<Distribution> for RouteParams {
    fn param(&self) -> Distribution {
        self.distribution.clone()
    }
}

impl svc::Param<profiles::http::Route> for RouteParams {
    fn param(&self) -> profiles::http::Route {
        self.profile.clone()
    }
}

impl svc::Param<metrics::ProfileRouteLabels> for RouteParams {
    fn param(&self) -> metrics::ProfileRouteLabels {
        metrics::ProfileRouteLabels::outbound(self.logical.logical_addr.clone(), &self.profile)
    }
}

impl svc::Param<http::ResponseTimeout> for RouteParams {
    fn param(&self) -> http::ResponseTimeout {
        http::ResponseTimeout(self.profile.timeout())
    }
}

impl classify::CanClassify for RouteParams {
    type Classify = classify::Request;

    fn classify(&self) -> classify::Request {
        self.profile.response_classes().clone().into()
    }
}
