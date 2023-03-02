use super::{super::concrete, Concrete, NoRoute};
use linkerd_app_core::{
    proxy::http::{self, balance},
    svc,
    transport::addrs::*,
    Addr, Error, Infallible, Result,
};
use linkerd_distribute as distribute;
use linkerd_http_route as http_route;
use linkerd_proxy_client_policy as policy;
use std::{fmt::Debug, hash::Hash, sync::Arc};

mod route;
#[cfg(test)]
mod tests;

pub use self::route::errors;
pub use linkerd_proxy_client_policy::ClientPolicy;

/// HTTP or gRPC policy routes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Routes {
    Http(HttpRoutes),
    Grpc(GrpcRoutes),
}

pub type HttpRoutes = RouterRoutes<http_route::http::MatchRequest, policy::http::Filter>;
pub type GrpcRoutes = RouterRoutes<http_route::grpc::MatchRoute, policy::grpc::Filter>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouterRoutes<M, F> {
    pub addr: Addr,
    pub routes: Arc<[http_route::Route<M, policy::RoutePolicy<F>>]>,
    pub backends: Arc<[policy::Backend]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Params<T: Clone + Debug + Eq + Hash> {
    Http(HttpParams<T>),
    Grpc(GrpcParams<T>),
}

pub(super) type HttpParams<T> =
    RouterParams<T, http_route::http::MatchRequest, policy::http::Filter>;
pub(super) type GrpcParams<T> = RouterParams<T, http_route::grpc::MatchRoute, policy::grpc::Filter>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RouterParams<T: Clone + Debug + Eq + Hash, M, F> {
    parent: T,
    addr: Addr,
    routes: Arc<[http_route::Route<M, route::Route<T, F>>]>,
    backends: distribute::Backends<Concrete<T>>,
}

type NewBackendCache<T, N, S> = distribute::NewBackendCache<Concrete<T>, (), N, S>;

// === impl Routes ===

impl Routes {
    pub fn addr(&self) -> &Addr {
        match self {
            Routes::Http(RouterRoutes { ref addr, .. })
            | Routes::Grpc(RouterRoutes { ref addr, .. }) => addr,
        }
    }
}

// === impl Params ===

impl<T> Params<T>
where
    // Parent target type.
    T: Clone + Debug + Eq + Hash + Send + Sync + 'static,
{
    /// Wraps an HTTP `NewService` with HTTP or gRPC policy routing layers.
    pub(super) fn layer<N, S>() -> impl svc::Layer<
        N,
        Service = svc::ArcNewService<
            Self,
            impl svc::Service<
                    http::Request<http::BoxBody>,
                    Response = http::Response<http::BoxBody>,
                    Error = Error,
                    Future = impl Send,
                > + Clone,
        >,
    > + Clone
    where
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
            let http = svc::stack(inner.clone()).push(HttpParams::layer());
            let grpc = svc::stack(inner).push(GrpcParams::layer());

            http.push_switch(
                |pp: Params<T>| {
                    Ok::<_, Infallible>(match pp {
                        Self::Http(http) => svc::Either::A(http),
                        Self::Grpc(grpc) => svc::Either::B(grpc),
                    })
                },
                grpc.into_inner(),
            )
            .push(svc::ArcNewService::layer())
            .into_inner()
        })
    }
}

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

// === impl RouterParams ===

impl<T, M, F> RouterParams<T, M, F>
where
    // Parent target type.
    T: Clone + Debug + Eq + Hash + Send + Sync + 'static,
    // Request matcher.
    M: http_route::Match,
    M: Clone + Send + Sync + 'static,
    M::Summary: Clone + Debug + Eq + Hash + Send + Sync + 'static,
    // Request filter.
    F: Debug + Eq + Hash,
    F: Clone + Send + Sync + 'static,
    // Assert that we can route for the given match and filter types.
    Self: svc::router::SelectRoute<
        http::Request<http::BoxBody>,
        Key = route::MatchedRoute<T, M::Summary, F>,
        Error = NoRoute,
    >,
    route::MatchedRoute<T, M::Summary, F>: route::filters::Apply,
    route::MatchedBackend<T, M::Summary, F>: route::filters::Apply,
{
    /// Wraps a `NewService`--instantiated once per logical target--that caches a set
    /// of concrete services so that, as the watch provides new `Params`, we can
    /// reuse inner services.
    fn layer<N, S>() -> impl svc::Layer<
        N,
        Service = svc::ArcNewService<
            Self,
            impl svc::Service<
                    http::Request<http::BoxBody>,
                    Response = http::Response<http::BoxBody>,
                    Error = Error,
                    Future = impl Send,
                > + Clone,
        >,
    > + Clone
    where
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
                .lift_new()
                .push(NewBackendCache::layer())
                // Lazily cache a service for each `RouteParams` returned from the
                // `SelectRoute` impl.
                .push_on_service(route::MatchedRoute::layer())
                .push(svc::NewOneshotRoute::<Self, (), _>::layer_cached())
                .push(svc::ArcNewService::layer())
                .into_inner()
        })
    }
}

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

        let mk_dispatch = move |bd: &policy::BackendDispatcher| match *bd {
            policy::BackendDispatcher::BalanceP2c(
                policy::Load::PeakEwma(policy::PeakEwma { decay, default_rtt }),
                policy::EndpointDiscovery::DestinationGet { ref path },
            ) => mk_concrete(concrete::Dispatch::Balance(
                path.parse().expect("destination must be a nameaddr"),
                balance::EwmaConfig { decay, default_rtt },
            )),
            policy::BackendDispatcher::Forward(addr, ref metadata) => mk_concrete(
                concrete::Dispatch::Forward(Remote(ServerAddr(addr)), metadata.clone()),
            ),
        };

        let mk_route_backend = |rb: &policy::RouteBackend<F>| {
            let filters = rb.filters.clone();
            let concrete = mk_dispatch(&rb.backend.dispatcher);
            route::Backend { filters, concrete }
        };

        let mk_distribution = |d: &policy::RouteDistribution<F>| match d {
            policy::RouteDistribution::Empty => route::Distribution::Empty,
            policy::RouteDistribution::FirstAvailable(backends) => {
                route::Distribution::first_available(backends.iter().map(mk_route_backend))
            }
            policy::RouteDistribution::RandomAvailable(backends) => {
                route::Distribution::random_available(
                    backends
                        .iter()
                        .map(|(rb, weight)| (mk_route_backend(rb), *weight)),
                )
                .expect("distribution must be valid")
            }
        };

        let mk_policy = |policy::RoutePolicy::<F> {
                             meta,
                             filters,
                             distribution,
                         }| route::Route {
            addr: addr.clone(),
            parent: parent.clone(),
            meta,
            filters,
            distribution: mk_distribution(&distribution),
        };

        let routes = routes
            .iter()
            .map(|route| http_route::Route {
                hosts: route.hosts.clone(),
                rules: route
                    .rules
                    .iter()
                    .cloned()
                    .map(|http_route::Rule { matches, policy }| http_route::Rule {
                        matches,
                        policy: mk_policy(policy),
                    })
                    .collect(),
            })
            .collect();

        let backends = backends
            .iter()
            .map(|t| mk_dispatch(&t.dispatcher))
            .collect();

        Self {
            routes,
            backends,
            addr,
            parent,
        }
    }
}

impl<B, T> svc::router::SelectRoute<http::Request<B>>
    for RouterParams<T, http_route::http::MatchRequest, policy::http::Filter>
where
    T: Eq + Hash + Clone + Debug,
{
    type Key = route::HttpMatchedRoute<T>;
    type Error = NoRoute;

    fn select(&self, req: &http::Request<B>) -> Result<Self::Key, Self::Error> {
        let (r#match, params) = policy::http::find(&*self.routes, req).ok_or(NoRoute)?;
        tracing::debug!(?r#match, ?params, uri = ?req.uri(), headers = ?req.headers(), "Selecting route");
        Ok(route::Matched {
            r#match,
            params: params.clone(),
        })
    }
}

impl<T, B> svc::router::SelectRoute<http::Request<B>>
    for RouterParams<T, http_route::grpc::MatchRoute, policy::grpc::Filter>
where
    T: Eq + Hash + Clone + Debug,
{
    type Key = route::GrpcMatchedRoute<T>;
    type Error = NoRoute;

    fn select(&self, req: &http::Request<B>) -> Result<Self::Key, Self::Error> {
        let (r#match, params) = policy::grpc::find(&*self.routes, req).ok_or(NoRoute)?;
        Ok(route::Matched {
            r#match,
            params: params.clone(),
        })
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

impl<T, M, F> svc::Param<distribute::Backends<Concrete<T>>> for RouterParams<T, M, F>
where
    T: Eq + Hash + Clone + Debug,
{
    fn param(&self) -> distribute::Backends<Concrete<T>> {
        self.backends.clone()
    }
}
