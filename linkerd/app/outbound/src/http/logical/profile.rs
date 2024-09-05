use super::{
    super::{concrete, retry},
    CanonicalDstHeader, Concrete, NoRoute,
};
use crate::{policy, BackendRef, ParentRef};
use linkerd_app_core::{
    classify, metrics,
    proxy::http::{self, balance},
    svc, Error, NameAddr,
};
use linkerd_distribute as distribute;
use std::{fmt::Debug, hash::Hash, sync::Arc, time};
use tokio::sync::watch;

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

type NewBackendCache<T, N, S> = distribute::NewBackendCache<Concrete<T>, (), N, S>;
type NewDistribute<T, N> = distribute::NewDistribute<Concrete<T>, (), N>;
type Distribution<T> = distribute::Distribution<Concrete<T>>;

pub(crate) const DEFAULT_EWMA: balance::EwmaConfig = balance::EwmaConfig {
    default_rtt: time::Duration::from_millis(30),
    decay: time::Duration::from_secs(10),
};

pub(crate) fn should_override_policy(rx: &watch::Receiver<Profile>) -> Option<LogicalAddr> {
    let p = rx.borrow();
    if p.has_routes_or_targets() {
        p.addr.clone()
    } else {
        None
    }
}

// === impl Params ===

impl<T> Params<T>
where
    T: Clone + Debug + Eq + Hash + Send + Sync + 'static,
{
    /// Wraps a `NewService`--instantiated once per logical target--that caches
    /// a set of concrete services so that, as the watch provides new `Params`,
    /// we can reuse inner services.
    pub(super) fn layer<N, S>(
        metrics: metrics::Proxy,
    ) -> impl svc::Layer<N, Service = svc::ArcNewCloneHttp<Self>> + Clone
    where
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
                .lift_new()
                .push(NewBackendCache::layer())
                // Lazily cache a service for each `RouteParams`
                // returned from the `SelectRoute` impl.
                .push_on_service(RouteParams::layer(metrics.clone()))
                .push(svc::NewOneshotRoute::<Params<T>, _, _>::layer_cached())
                .arc_new_clone_http()
                .into_inner()
        })
    }
}

static UNKNOWN_META: once_cell::sync::Lazy<Arc<policy::Meta>> =
    once_cell::sync::Lazy::new(|| policy::Meta::new_default("unknown"));

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

        fn service_meta(addr: &NameAddr) -> Option<Arc<policy::Meta>> {
            let mut parts = addr.name().split('.');

            let name = parts.next()?;
            let namespace = parts.next()?;

            if !parts.next()?.eq_ignore_ascii_case("svc") {
                return None;
            }

            Some(Arc::new(policy::Meta::Resource {
                group: "core".to_string(),
                kind: "Service".to_string(),
                namespace: namespace.to_string(),
                name: name.to_string(),
                section: None,
                port: Some(addr.port().try_into().ok()?),
            }))
        }

        let parent_meta = service_meta(&addr).unwrap_or_else(|| UNKNOWN_META.clone());

        // Create concrete targets for all of the profile's routes.
        let (backends, distribution) = if targets.is_empty() {
            let concrete = Concrete {
                parent_ref: ParentRef(parent_meta.clone()),
                backend_ref: BackendRef(parent_meta),
                target: concrete::Dispatch::Balance(addr.clone(), DEFAULT_EWMA),
                authority: Some(addr.as_http_authority()),
                parent: parent.clone(),
                failure_accrual: Default::default(),
            };
            let backends = std::iter::once(concrete.clone()).collect();
            let distribution = Distribution::first_available(std::iter::once(concrete));
            (backends, distribution)
        } else {
            let backends = targets
                .iter()
                .map(|t| Concrete {
                    parent_ref: ParentRef(parent_meta.clone()),
                    backend_ref: BackendRef(
                        service_meta(&t.addr).unwrap_or_else(|| UNKNOWN_META.clone()),
                    ),
                    target: concrete::Dispatch::Balance(t.addr.clone(), DEFAULT_EWMA),
                    authority: Some(t.addr.as_http_authority()),
                    parent: parent.clone(),
                    failure_accrual: Default::default(),
                })
                .collect();
            let distribution = Distribution::random_available(targets.iter().cloned().map(
                |Target { addr, weight }| {
                    let concrete = Concrete {
                        parent_ref: ParentRef(parent_meta.clone()),
                        backend_ref: BackendRef(
                            service_meta(&addr).unwrap_or_else(|| UNKNOWN_META.clone()),
                        ),
                        authority: Some(addr.as_http_authority()),
                        target: concrete::Dispatch::Balance(addr, DEFAULT_EWMA),
                        parent: parent.clone(),
                        failure_accrual: Default::default(),
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
        route_for_request(&self.profile_routes, req)
            .ok_or(NoRoute)
            .cloned()
    }
}

// === impl RouteParams ===

impl<T> RouteParams<T> {
    fn layer<N, S>(
        metrics: metrics::Proxy,
    ) -> impl svc::Layer<N, Service = svc::ArcNewCloneHttp<Self>> + Clone
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
                .lift_new()
                .push(NewDistribute::layer())
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
                // Add l5d-dst-canonical header to requests.
                //
                // TODO(ver) move this into the endpoint stack so that we can
                // only set this on meshed connections.
                .push(
                    http::NewHeaderFromTarget::<CanonicalDstHeader, _, _>::layer_via(
                        |rp: &Self| {
                            let LogicalAddr(addr) = rp.addr.clone();
                            CanonicalDstHeader(addr)
                        },
                    ),
                )
                // Sets the per-route response classifier as a request
                // extension.
                .push(classify::NewClassify::layer())
                .push_on_service(http::BoxResponse::layer())
                .arc_new_clone_http()
                .into_inner()
        })
    }
}

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
