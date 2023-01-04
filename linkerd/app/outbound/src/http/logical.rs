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

            concrete
                .check_new_service::<Concrete, _>()
                // For each `Logical` target, build a stack that caches a
                // `Concrete` inner services to provide a distributor to the
                // router.
                //
                // Each `RouteParams` provides a `Distribution` that is used to
                // choose a concrete service for a given route.
                .push(svc::layer::mk(svc::NewCloneService::from))
                .push_on_service(
                    svc::layers()
                        .push(CacheNewDistribute::layer())
                        .push(router::NewRoute::layer_cached::<RouteParams>(route)),
                )
                .check_new_new::<Logical, Params>()
                // Watch the `Profile` for each target. Every time it changes,
                // build a new stack by converting the profile to a `Params`.
                .push(svc::NewSpawnWatch::<Profile, _>::layer_into::<Params>())
                .check_new_service::<Logical, http::Request<http::BoxBody>>()
                // Strips headers that may be set by this proxy and add an
                // outbound canonical-dst-header. The response body is boxed
                // unify the profile stack's response type with that of to
                // endpoint stack.
                .push(http::NewHeaderFromTarget::<CanonicalDstHeader, _>::layer())
                // This caches each logical stack so that it can be reused
                // across per-connection HTTP server stacks (i.e. created by the
                // `DetectService`).
                //
                // TODO(ver) Update the detection stack so this dynamic caching
                // can be removed.
                //
                // XXX(ver) This cache key includes the HTTP version. Should it?
                .push_cache(config.discovery_idle_timeout)
                .instrument(|l: &Logical| debug_span!("logical", service = %l.logical_addr))
                .push(svc::ArcNewService::layer())
        })
    }
}

// === impl Params ===

impl From<(Profile, Logical)> for Params {
    fn from((profile, logical): (Profile, Logical)) -> Self {
        // Create concrete targets for all of the profile's
        let backends = profile
            .target_addrs
            .iter()
            .map(|addr| super::Concrete {
                resolve: ConcreteAddr(addr.clone()),
                logical: logical.clone(),
            })
            .collect();

        let routes = profile
            .http_routes
            .iter()
            .cloned()
            .map(|(req_match, profile)| {
                let distribution =
                    Distribution::random_available(profile.targets().iter().cloned().map(
                        |profiles::Target { addr, weight }| {
                            let concrete = super::Concrete {
                                logical: logical.clone(),
                                resolve: ConcreteAddr(addr),
                            };
                            (concrete, weight)
                        },
                    ))
                    .expect("distribution must be valid");
                let params = RouteParams {
                    logical: logical.clone(),
                    distribution,
                    profile,
                };
                (req_match, params)
            })
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

    fn select<'r>(&self, req: &'r http::Request<B>) -> Result<&Self::Key, Self::Error> {
        profiles::http::route_for_request(&*self.routes, req).ok_or(NoRoute)
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
