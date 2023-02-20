use super::{super::concrete, BackendCache, Concrete, Distribution, NoRoute};
use linkerd_app_core::{
    proxy::http::{self, balance},
    svc,
    transport::addrs::*,
    Addr, Error,
};
use linkerd_distribute as distribute;
use linkerd_http_route as route;
use linkerd_proxy_client_policy as policy;
use std::{fmt::Debug, hash::Hash, sync::Arc};

pub use linkerd_proxy_client_policy::ClientPolicy;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Routes<M, F> {
    pub addr: Addr,
    pub routes: Arc<[route::Route<M, policy::RoutePolicy<F>>]>,
    pub backends: Arc<[policy::Backend]>,
}

pub type HttpRoutes = Routes<route::http::MatchRequest, policy::http::Filter>;
pub type GrpcRoutes = Routes<route::grpc::MatchRoute, policy::grpc::Filter>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Params<T: Clone + Debug + Eq + Hash, M, F> {
    parent: T,
    addr: Addr,
    routes: Arc<[route::Route<M, RouteParams<T, F>>]>,
    backends: distribute::Backends<Concrete<T>>,
}

pub(super) type HttpParams<T> = Params<T, route::http::MatchRequest, policy::http::Filter>;
pub(super) type GrpcParams<T> = Params<T, route::grpc::MatchRoute, policy::grpc::Filter>;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct RouteParams<T, F> {
    parent: T,
    addr: Addr,
    meta: Arc<policy::Meta>,
    filters: Arc<[F]>,
    distribution: Distribution<T>,
}

// Wraps a `NewService`--instantiated once per logical target--that caches a set
// of concrete services so that, as the watch provides new `Params`, we can
// reuse inner services.
#[allow(dead_code)]
pub(super) fn layer<T, M, F, N, S>() -> impl svc::Layer<
    N,
    Service = svc::ArcNewService<
        Params<T, M, F>,
        impl svc::Service<
                http::Request<http::BoxBody>,
                Response = http::Response<http::BoxBody>,
                Error = Error,
                Future = impl Send,
            > + Clone,
    >,
> + Clone
where
    // Parent target type.
    T: Clone + Debug + Eq + Hash + Send + Sync + 'static,
    // Request matcher.
    M: Clone + Send + Sync + 'static,
    // Request filter.
    F: Eq + Hash,
    F: Clone + Send + Sync + 'static,
    // Assert that we can route the provided `Params`.
    Params<T, M, F>: svc::router::SelectRoute<
        http::Request<http::BoxBody>,
        Key = RouteParams<T, F>,
        Error = NoRoute,
    >,
    // Inner stack.
    N: svc::NewService<Concrete<T>, Service = S>,
    N: Clone + Send + Sync + 'static,
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
            // Lazily cache a service for each `RouteParams` returned from the
            // `SelectRoute` impl.
            .push_on_service(route_layer())
            .push(svc::NewOneshotRoute::<Params<T, M, F>, (), _>::layer_cached())
            .push(svc::ArcNewService::layer())
            .into_inner()
    })
}

fn route_layer<T, F, N, S>() -> impl svc::Layer<
    N,
    Service = svc::ArcNewService<
        RouteParams<T, F>,
        impl svc::Service<
                http::Request<http::BoxBody>,
                Response = http::Response<http::BoxBody>,
                Error = Error,
                Future = impl Send,
            > + Clone,
    >,
> + Clone
where
    // Parent target.
    T: Send + Sync + 'static,
    // Request filter.
    F: Clone + Send + Sync + 'static,
    // Inner stack.
    N: svc::NewService<RouteParams<T, F>, Service = S>,
    N: Clone + Send + Sync + 'static,
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
            // The router does not take the backend's availability into
            // consideration, so we must eagerly fail requests to prevent
            // leaking tasks onto the runtime.
            .push_on_service(svc::LoadShed::layer())
            .push(svc::ArcNewService::layer())
            .into_inner()
    })
}

// === impl Params ===

impl<T, M, F> From<(Routes<M, F>, T)> for Params<T, M, F>
where
    T: Eq + Hash + Clone + Debug,
    M: Clone,
    F: Clone,
{
    fn from((rts, parent): (Routes<M, F>, T)) -> Self {
        let Routes {
            addr,
            routes,
            backends,
        } = rts;

        let mk_concrete = {
            let authority = addr.to_http_authority();
            let parent = parent.clone();
            move |target: concrete::Dispatch| Concrete {
                target,
                authority: Some(authority.clone()),
                parent: parent.clone(),
            }
        };

        let routes = routes
            .iter()
            .map(|route| {
                let rules = route.rules.iter().map(
                    |route::Rule { matches, policy }| -> route::Rule<M, RouteParams<T, F>> {
                        let filters = policy.filters.clone();
                        let distribution = match policy.distribution.clone() {
                            policy::RouteDistribution::Empty => Distribution::Empty,

                            policy::RouteDistribution::FirstAvailable(backends) => {
                                Distribution::first_available(backends.iter().map(|rb| {
                                    match rb.backend.dispatcher.clone() {
                                        policy::BackendDispatcher::BalanceP2c(
                                            policy::Load::PeakEwma(policy::PeakEwma {
                                                decay,
                                                default_rtt,
                                            }),
                                            policy::EndpointDiscovery::DestinationGet { path },
                                        ) => mk_concrete(concrete::Dispatch::Balance(
                                            path.parse().expect("destination must be a nameaddr"),
                                            balance::EwmaConfig { decay, default_rtt },
                                        )),

                                        policy::BackendDispatcher::Forward(addr, metadata) => {
                                            mk_concrete(concrete::Dispatch::Forward(
                                                Remote(ServerAddr(addr)),
                                                metadata,
                                            ))
                                        }
                                    }
                                }))
                            }

                            policy::RouteDistribution::RandomAvailable(backends) => {
                                Distribution::random_available(backends.iter().map(
                                    |(rb, weight)| {
                                        let concrete = match rb.backend.dispatcher.clone() {
                                            policy::BackendDispatcher::BalanceP2c(
                                                policy::Load::PeakEwma(policy::PeakEwma {
                                                    decay,
                                                    default_rtt,
                                                }),
                                                policy::EndpointDiscovery::DestinationGet { path },
                                            ) => mk_concrete(concrete::Dispatch::Balance(
                                                path.parse()
                                                    .expect("destination must be a nameaddr"),
                                                balance::EwmaConfig { decay, default_rtt },
                                            )),

                                            policy::BackendDispatcher::Forward(ep, metadata) => {
                                                mk_concrete(concrete::Dispatch::Forward(
                                                    Remote(ServerAddr(ep)),
                                                    metadata,
                                                ))
                                            }
                                        };

                                        (concrete, *weight)
                                    },
                                ))
                                .expect("distribution must be valid")
                            }
                        };

                        route::Rule {
                            matches: matches.clone(),
                            policy: RouteParams {
                                addr: addr.clone(),
                                parent: parent.clone(),
                                meta: policy.meta.clone(),
                                filters,
                                distribution,
                            },
                        }
                    },
                );

                route::Route {
                    hosts: route.hosts.clone(),
                    rules: rules.collect(),
                }
            })
            .collect::<Arc<[_]>>();

        let backends = backends
            .iter()
            .map(|t| match t.dispatcher.clone() {
                policy::BackendDispatcher::BalanceP2c(
                    policy::Load::PeakEwma(policy::PeakEwma { decay, default_rtt }),
                    policy::EndpointDiscovery::DestinationGet { path },
                ) => mk_concrete(concrete::Dispatch::Balance(
                    path.parse().expect("destination must be a nameaddr"),
                    balance::EwmaConfig { decay, default_rtt },
                )),
                policy::BackendDispatcher::Forward(addr, metadata) => mk_concrete(
                    concrete::Dispatch::Forward(Remote(ServerAddr(addr)), metadata),
                ),
            })
            .collect();

        Self {
            addr,
            parent,
            routes,
            backends,
        }
    }
}

impl<T, M, F> svc::Param<distribute::Backends<Concrete<T>>> for Params<T, M, F>
where
    T: Eq + Hash + Clone + Debug,
{
    fn param(&self) -> distribute::Backends<Concrete<T>> {
        self.backends.clone()
    }
}

impl<B, T> svc::router::SelectRoute<http::Request<B>>
    for Params<T, route::http::MatchRequest, policy::http::Filter>
where
    T: Eq + Hash + Clone + Debug,
{
    type Key = RouteParams<T, policy::http::Filter>;
    type Error = NoRoute;

    fn select(&self, req: &http::Request<B>) -> Result<Self::Key, Self::Error> {
        let (_match, params) = policy::http::find(&*self.routes, req).ok_or(NoRoute)?;
        Ok(params.clone())
    }
}

impl<T, B> svc::router::SelectRoute<http::Request<B>>
    for Params<T, route::grpc::MatchRoute, policy::grpc::Filter>
where
    T: Eq + Hash + Clone + Debug,
{
    type Key = RouteParams<T, policy::grpc::Filter>;
    type Error = NoRoute;

    fn select(&self, req: &http::Request<B>) -> Result<Self::Key, Self::Error> {
        let (_match, params) = policy::grpc::find(&*self.routes, req).ok_or(NoRoute)?;
        Ok(params.clone())
    }
}

impl<T, M, F> svc::Param<super::LogicalAddr> for Params<T, M, F>
where
    T: Eq + Hash + Clone + Debug,
{
    fn param(&self) -> super::LogicalAddr {
        super::LogicalAddr(self.addr.clone())
    }
}

// === impl RouteParams ===

impl<T: Clone, F> svc::Param<Distribution<T>> for RouteParams<T, F> {
    fn param(&self) -> Distribution<T> {
        self.distribution.clone()
    }
}
