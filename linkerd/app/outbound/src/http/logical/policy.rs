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
pub struct Routes {
    pub addr: Addr,
    pub routes: Arc<[policy::http::Route]>,
    pub backends: Arc<[policy::Backend]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Params<T: Clone + Debug + Eq + Hash> {
    parent: T,
    addr: Addr,
    routes: Arc<[route::http::Route<RouteParams<T>>]>,
    backends: distribute::Backends<Concrete<T>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct RouteParams<T> {
    parent: T,
    addr: Addr,
    meta: Arc<policy::Meta>,
    filters: Arc<[policy::http::Filter]>,
    distribution: Distribution<T>,
}

// Wraps a `NewService`--instantiated once per logical target--that caches a set
// of concrete services so that, as the watch provides new `Params`, we can
// reuse inner services.
#[allow(dead_code)]
pub(super) fn layer<T, N, S>() -> impl svc::Layer<
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
            .push_on_service(route_layer())
            .push(svc::NewOneshotRoute::<Params<T>, _, _>::layer_cached())
            .push(svc::ArcNewService::layer())
            .into_inner()
    })
}

fn route_layer<T, N, S>() -> impl svc::Layer<
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
            // The router does not take the backend's availability into
            // consideration, so we must eagerly fail requests to prevent
            // leaking tasks onto the runtime.
            .push_on_service(svc::LoadShed::layer())
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
        let addr = routes.addr.clone();

        let backends = routes
            .backends
            .iter()
            .map(|t| match t.dispatcher.clone() {
                policy::BackendDispatcher::BalanceP2c(
                    policy::Load::PeakEwma(policy::PeakEwma { decay, default_rtt }),
                    policy::EndpointDiscovery::DestinationGet { path },
                ) => Concrete {
                    target: concrete::Dispatch::Balance(
                        path.parse().expect("destination must be a nameaddr"),
                        balance::EwmaConfig { decay, default_rtt },
                    ),
                    authority: None, // FIXME?
                    parent: parent.clone(),
                },
                policy::BackendDispatcher::Forward(addr, metadata) => Concrete {
                    target: concrete::Dispatch::Forward(Remote(ServerAddr(addr)), metadata),
                    authority: None,
                    parent: parent.clone(),
                },
            })
            .collect();

        let routes = routes
            .routes
            .iter()
            .map(|route| {
                let rules = route
                    .rules
                    .iter()
                    .map(|route::http::Rule { matches, policy }| {
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
                                        ) => Concrete {
                                            target: concrete::Dispatch::Balance(
                                                path.parse()
                                                    .expect("destination must be a nameaddr"),
                                                balance::EwmaConfig { decay, default_rtt },
                                            ),
                                            authority: None, // FIXME?
                                            parent: parent.clone(),
                                        },
                                        policy::BackendDispatcher::Forward(addr, metadata) => {
                                            Concrete {
                                                target: concrete::Dispatch::Forward(
                                                    Remote(ServerAddr(addr)),
                                                    metadata,
                                                ),
                                                authority: None,
                                                parent: parent.clone(),
                                            }
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
                                            ) => Concrete {
                                                target: concrete::Dispatch::Balance(
                                                    path.parse()
                                                        .expect("destination must be a nameaddr"),
                                                    balance::EwmaConfig { decay, default_rtt },
                                                ),
                                                authority: Some(addr.to_http_authority()),
                                                parent: parent.clone(),
                                            },
                                            policy::BackendDispatcher::Forward(ep, metadata) => {
                                                Concrete {
                                                    target: concrete::Dispatch::Forward(
                                                        Remote(ServerAddr(ep)),
                                                        metadata,
                                                    ),
                                                    authority: Some(addr.to_http_authority()),
                                                    parent: parent.clone(),
                                                }
                                            }
                                        };
                                        (concrete, *weight)
                                    },
                                ))
                                .expect("distribution must be valid")
                            }
                        };

                        route::http::Rule {
                            matches: matches.clone(),
                            policy: RouteParams {
                                addr: addr.clone(),
                                parent: parent.clone(),
                                meta: policy.meta.clone(),
                                filters: policy.filters.clone(),
                                distribution,
                            },
                        }
                    });

                route::http::Route {
                    hosts: route.hosts.clone(),
                    rules: rules.collect(),
                }
            })
            .collect::<Arc<[_]>>();

        Self {
            addr,
            parent,
            routes,
            backends,
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

impl<T, B> svc::router::SelectRoute<http::Request<B>> for Params<T>
where
    T: Eq + Hash + Clone + Debug,
{
    type Key = RouteParams<T>;
    type Error = NoRoute;

    fn select(&self, req: &http::Request<B>) -> Result<Self::Key, Self::Error> {
        let (_match, params) = policy::http::find(&*self.routes, req).ok_or(NoRoute)?;
        Ok(params.clone())
    }
}

impl<T> svc::Param<super::LogicalAddr> for Params<T>
where
    T: Eq + Hash + Clone + Debug,
{
    fn param(&self) -> super::LogicalAddr {
        super::LogicalAddr(self.addr.clone())
    }
}

// === impl RouteParams ===

impl<T: Clone> svc::Param<Distribution<T>> for RouteParams<T> {
    fn param(&self) -> Distribution<T> {
        self.distribution.clone()
    }
}
