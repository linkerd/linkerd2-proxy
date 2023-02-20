use super::{super::concrete, BackendCache, Concrete, Distribution, NoRoute};
use linkerd_app_core::{
    proxy::http::{self, balance},
    svc,
    transport::addrs::*,
    Addr, Error, Infallible,
};
use linkerd_distribute as distribute;
use linkerd_http_route as route;
use linkerd_proxy_client_policy as policy;
use std::{fmt::Debug, hash::Hash, sync::Arc};

#[cfg(test)]
mod tests;

pub use linkerd_proxy_client_policy::ClientPolicy;

/// HTTP or gRPC policy routes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Routes {
    Http(HttpRoutes),
    Grpc(GrpcRoutes),
}

pub type HttpRoutes = RouterRoutes<route::http::MatchRequest, policy::http::Filter>;
pub type GrpcRoutes = RouterRoutes<route::grpc::MatchRoute, policy::grpc::Filter>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouterRoutes<M, F> {
    pub addr: Addr,
    pub routes: Arc<[route::Route<M, policy::RoutePolicy<F>>]>,
    pub backends: Arc<[policy::Backend]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Params<T: Clone + Debug + Eq + Hash> {
    Http(HttpParams<T>),
    Grpc(GrpcParams<T>),
}

pub(super) type HttpParams<T> = RouterParams<T, route::http::MatchRequest, policy::http::Filter>;
pub(super) type GrpcParams<T> = RouterParams<T, route::grpc::MatchRoute, policy::grpc::Filter>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RouterParams<T: Clone + Debug + Eq + Hash, M, F> {
    parent: T,
    addr: Addr,
    routes: Arc<[route::Route<M, RouteParams<T, F>>]>,
    backends: distribute::Backends<Concrete<T>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct RouteParams<T, F> {
    parent: T,
    addr: Addr,
    meta: Arc<policy::Meta>,
    filters: Arc<[F]>,
    distribution: Distribution<T>,
}

/// HTTP or gRPC policy routing.
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
    // Parent target type.
    T: Clone + Debug + Eq + Hash + Send + Sync + 'static,
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
    svc::layer::mk(|inner: N| {
        let http = svc::stack(inner.clone()).push(routes_layer());
        let grpc = svc::stack(inner).push(routes_layer());

        http.push_switch(
            |pp: Params<T>| {
                Ok::<_, Infallible>(match pp {
                    Params::Http(http) => svc::Either::A(http),
                    Params::Grpc(grpc) => svc::Either::B(grpc),
                })
            },
            grpc.into_inner(),
        )
        .push(svc::ArcNewService::layer())
        .into_inner()
    })
}

// Wraps a `NewService`--instantiated once per logical target--that caches a set
// of concrete services so that, as the watch provides new `Params`, we can
// reuse inner services.
fn routes_layer<T, M, F, N, S>() -> impl svc::Layer<
    N,
    Service = svc::ArcNewService<
        RouterParams<T, M, F>,
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
    RouterParams<T, M, F>: svc::router::SelectRoute<
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
    svc::layer::mk(|inner| {
        svc::stack(inner)
            // Each `RouteParams` provides a `Distribution` that is used to
            // choose a concrete service for a given route.
            .push(BackendCache::layer())
            // Lazily cache a service for each `RouteParams` returned from the
            // `SelectRoute` impl.
            .push_on_service(route_layer())
            .push(svc::NewOneshotRoute::<RouterParams<T, M, F>, (), _>::layer_cached())
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
    svc::layer::mk(|inner| {
        svc::stack(inner)
            // The router does not take the backend's availability into
            // consideration, so we must eagerly fail requests to prevent
            // leaking tasks onto the runtime.
            .push_on_service(svc::LoadShed::layer())
            .push(svc::ArcNewService::layer())
            .into_inner()
    })
}

// === impl PolicyRoutes ===

impl Routes {
    pub fn addr(&self) -> &Addr {
        match self {
            Routes::Http(RouterRoutes { ref addr, .. })
            | Routes::Grpc(RouterRoutes { ref addr, .. }) => addr,
        }
    }
}

// === impl PolicyParams ===

impl<T> From<(Routes, T)> for Params<T>
where
    T: Eq + Hash + Clone + Debug,
{
    fn from((pr, parent): (Routes, T)) -> Self {
        match pr {
            Routes::Http(http) => Params::Http(RouterParams::from((http, parent))),
            Routes::Grpc(grpc) => Params::Grpc(RouterParams::from((grpc, parent))),
        }
    }
}

impl<T> svc::Param<super::LogicalAddr> for Params<T>
where
    T: Eq + Hash + Clone + Debug,
{
    fn param(&self) -> super::LogicalAddr {
        match self {
            Params::Http(p) => p.param(),
            Params::Grpc(p) => p.param(),
        }
    }
}

// === impl Params ===

impl<T, M, F> From<(RouterRoutes<M, F>, T)> for RouterParams<T, M, F>
where
    T: Eq + Hash + Clone + Debug,
    M: Clone,
    F: Clone,
{
    fn from((rts, parent): (RouterRoutes<M, F>, T)) -> Self {
        let RouterRoutes {
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

impl<T, M, F> svc::Param<distribute::Backends<Concrete<T>>> for RouterParams<T, M, F>
where
    T: Eq + Hash + Clone + Debug,
{
    fn param(&self) -> distribute::Backends<Concrete<T>> {
        self.backends.clone()
    }
}

impl<B, T> svc::router::SelectRoute<http::Request<B>>
    for RouterParams<T, route::http::MatchRequest, policy::http::Filter>
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
    for RouterParams<T, route::grpc::MatchRoute, policy::grpc::Filter>
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

impl<T, M, F> svc::Param<super::LogicalAddr> for RouterParams<T, M, F>
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
