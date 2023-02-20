use super::{
    super::{concrete, retry},
    BackendCache, Concrete, Distribution, NoRoute,
};
use linkerd_app_core::{
    classify, metrics,
    proxy::http::{self, balance},
    svc, Error,
};
use linkerd_distribute as distribute;
use std::{fmt::Debug, hash::Hash, sync::Arc, time};

pub use linkerd_app_core::profiles::{
    http::{route_for_request, RequestMatch, Route},
    LogicalAddr, Profile, Target,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Routes {
    pub addr: LogicalAddr,
    pub routes: Arc<[(RequestMatch, Route)]>,
    pub targets: Arc<[Target]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Params<T: Clone + Debug + Eq + Hash> {
    parent: T,
    addr: LogicalAddr,
    profile_routes: Arc<[(RequestMatch, RouteParams<T>)]>,
    backends: distribute::Backends<Concrete<T>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct RouteParams<T> {
    parent: T,
    addr: LogicalAddr,
    profile: Route,
    distribution: Distribution<T>,
}

// Wraps a `NewService`--instantiated once per logical target--that caches a set
// of concrete services so that, as the watch provides new `Params`, we can
// reuse inner services.
pub(super) fn layer<T, N, S>(
    metrics: metrics::Proxy,
) -> impl svc::Layer<
    N,
    Service = svc::ArcNewService<
        Params<T>,
        impl svc::Service<
                http::Request<http::BoxBody>,
                Response = http::Response<http::BoxBody>,
                Error = Error,
                Future = impl Send,
            > + Clone,
    >,
> + Clone
where
    T: Clone + Debug + Eq + Hash + Send + Sync + 'static,
    N: svc::NewService<Concrete<T>, Service = S> + Clone + Send + Sync + 'static,
    S: svc::Service<
        http::Request<http::BoxBody>,
        Response = http::Response<http::BoxBody>,
        Error = Error,
    >,
    S: Clone + Send + Sync + 'static,
    S::Future: Send,
{
    svc::layer::mk(move |inner| {
        svc::stack(inner)
            // Each `RouteParams` provides a `Distribution` that is used to
            // choose a concrete service for a given route.
            .push(BackendCache::layer())
            // Lazily cache a service for each `RouteParams`
            // returned from the `SelectRoute` impl.
            .push_on_service(route_layer(metrics.clone()))
            .push(svc::NewOneshotRoute::<Params<T>, _, _>::layer_cached())
            .push(svc::ArcNewService::layer())
            .into_inner()
    })
}

fn route_layer<T, N, S>(
    metrics: metrics::Proxy,
) -> impl svc::Layer<
    N,
    Service = svc::ArcNewService<
        RouteParams<T>,
        impl svc::Service<
                http::Request<http::BoxBody>,
                Response = http::Response<http::BoxBody>,
                Error = Error,
                Future = impl Send,
            > + Clone,
    >,
> + Clone
where
    T: Clone + Debug + Eq + Hash + Send + Sync + 'static,
    N: svc::NewService<RouteParams<T>, Service = S> + Clone + Send + Sync + 'static,
    S: svc::Service<
        http::Request<http::BoxBody>,
        Response = http::Response<http::BoxBody>,
        Error = Error,
    >,
    S: Clone + Send + Sync + 'static,
    S::Future: Send,
{
    svc::layer::mk(move |inner| {
        svc::stack(inner)
            .check_new_service::<RouteParams<T>, http::Request<http::BoxBody>>()
            .push_on_service(
                svc::layers()
                    .push(http::BoxRequest::layer())
                    // The router does not take the backend's availability into
                    // consideration, so we must eagerly fail requests to prevent
                    // leaking tasks onto the runtime.
                    .push(svc::LoadShed::layer()),
            )
            .push(http::insert::NewInsert::<Route, _>::layer())
            .push(
                metrics
                    .http_profile_route_actual
                    .to_layer::<classify::Response, _, RouteParams<T>>(),
            )
            // Depending on whether or not the request can be
            // retried, it may have one of two `Body` types. This
            // layer unifies any `Body` type into `BoxBody`.
            .push_on_service(http::BoxRequest::erased())
            // Sets an optional retry policy.
            .push(retry::layer(metrics.http_profile_route_retry.clone()))
            // Sets an optional request timeout.
            .push(http::NewTimeout::layer())
            // Records per-route metrics.
            .push(
                metrics
                    .http_profile_route
                    .to_layer::<classify::Response, _, RouteParams<T>>(),
            )
            // Sets the per-route response classifier as a request
            // extension.
            .push(classify::NewClassify::layer())
            // TODO(ver) .push(svc::NewMapErr::layer_from_target::<RouteError, _>())
            .push_on_service(http::BoxResponse::layer())
            .push(svc::ArcNewService::layer())
            .into_inner()
    })
}

// === impl Params ===

impl<T> From<(Routes, T)> for Params<T>
where
    T: Eq + Hash + Clone + Debug,
{
    fn from((routes, parent): (Routes, T)) -> Self {
        let Routes {
            addr: LogicalAddr(addr),
            routes,
            targets,
        } = routes;

        const EWMA: balance::EwmaConfig = balance::EwmaConfig {
            default_rtt: time::Duration::from_millis(30),
            decay: time::Duration::from_secs(10),
        };

        // Create concrete targets for all of the profile's routes.
        let (backends, distribution) = if targets.is_empty() {
            let concrete = Concrete {
                target: concrete::Dispatch::Balance(addr.clone(), EWMA),
                authority: Some(addr.as_http_authority()),
                parent: parent.clone(),
            };
            let backends = std::iter::once(concrete.clone()).collect();
            let distribution = Distribution::first_available(std::iter::once(concrete));
            (backends, distribution)
        } else {
            let backends = targets
                .iter()
                .map(|t| Concrete {
                    target: concrete::Dispatch::Balance(t.addr.clone(), EWMA),
                    authority: Some(t.addr.as_http_authority()),
                    parent: parent.clone(),
                })
                .collect();
            let distribution = Distribution::random_available(targets.iter().cloned().map(
                |Target { addr, weight }| {
                    let concrete = Concrete {
                        authority: Some(addr.as_http_authority()),
                        target: concrete::Dispatch::Balance(addr, EWMA),
                        parent: parent.clone(),
                    };
                    (concrete, weight)
                },
            ))
            .expect("distribution must be valid");

            (backends, distribution)
        };

        let profile_routes = routes
            .iter()
            .cloned()
            .map(|(req_match, profile)| {
                let params = RouteParams {
                    addr: LogicalAddr(addr.clone()),
                    profile,
                    parent: parent.clone(),
                    distribution: distribution.clone(),
                };
                (req_match, params)
            })
            // Add a default route.
            .chain(std::iter::once((
                RequestMatch::default(),
                RouteParams {
                    addr: LogicalAddr(addr.clone()),
                    profile: Default::default(),
                    parent: parent.clone(),
                    distribution: distribution.clone(),
                },
            )))
            .collect::<Arc<[(_, _)]>>();

        Self {
            addr: LogicalAddr(addr),
            parent,
            backends,
            profile_routes,
        }
    }
}

impl<T> svc::Param<distribute::Backends<Concrete<T>>> for Params<T>
where
    T: Eq + Hash + Clone + Debug,
{
    fn param(&self) -> distribute::Backends<Concrete<T>> {
        self.backends.clone()
    }
}

impl<T> svc::Param<LogicalAddr> for Params<T>
where
    T: Eq + Hash + Clone + Debug,
{
    fn param(&self) -> LogicalAddr {
        self.addr.clone()
    }
}

impl<T, B> svc::router::SelectRoute<http::Request<B>> for Params<T>
where
    T: Eq + Hash + Clone + Debug,
{
    type Key = RouteParams<T>;
    type Error = NoRoute;

    fn select(&self, req: &http::Request<B>) -> Result<Self::Key, Self::Error> {
        route_for_request(&*self.profile_routes, req)
            .ok_or(NoRoute)
            .cloned()
    }
}

// === impl RouteParams ===

impl<T> svc::Param<LogicalAddr> for RouteParams<T> {
    fn param(&self) -> LogicalAddr {
        self.addr.clone()
    }
}

impl<T: Clone> svc::Param<Distribution<T>> for RouteParams<T> {
    fn param(&self) -> Distribution<T> {
        self.distribution.clone()
    }
}

impl<T> svc::Param<Route> for RouteParams<T> {
    fn param(&self) -> Route {
        self.profile.clone()
    }
}

impl<T> svc::Param<metrics::ProfileRouteLabels> for RouteParams<T> {
    fn param(&self) -> metrics::ProfileRouteLabels {
        metrics::ProfileRouteLabels::outbound(self.addr.clone(), &self.profile)
    }
}

impl<T> svc::Param<http::ResponseTimeout> for RouteParams<T> {
    fn param(&self) -> http::ResponseTimeout {
        http::ResponseTimeout(self.profile.timeout())
    }
}

impl<T> svc::Param<classify::Request> for RouteParams<T> {
    fn param(&self) -> classify::Request {
        self.profile.response_classes().clone().into()
    }
}
