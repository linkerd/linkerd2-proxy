use super::{
    super::{concrete, Concrete, LogicalAddr, NoRoute},
    route,
};
use linkerd_app_core::{proxy::http, svc, transport::addrs::*, Addr, Error, Result};
use linkerd_distribute as distribute;
use linkerd_http_route as http_route;
use linkerd_proxy_client_policy as policy;
use std::{fmt::Debug, hash::Hash, sync::Arc};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Params<M, F> {
    pub addr: Addr,
    pub routes: Arc<[http_route::Route<M, policy::RoutePolicy<F>>]>,
    pub backends: Arc<[policy::Backend]>,
}

pub type HttpParams = Params<http_route::http::MatchRequest, policy::http::Filter>;
pub type GrpcParams = Params<http_route::grpc::MatchRoute, policy::grpc::Filter>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Router<T: Clone + Debug + Eq + Hash, M, F> {
    pub(super) parent: T,
    pub(super) addr: Addr,
    pub(super) routes: Arc<[http_route::Route<M, route::Route<T, F>>]>,
    pub(super) backends: distribute::Backends<Concrete<T>>,
}

pub(super) type Http<T> = Router<T, http_route::http::MatchRequest, policy::http::Filter>;
pub(super) type Grpc<T> = Router<T, http_route::grpc::MatchRoute, policy::grpc::Filter>;

type NewBackendCache<T, N, S> = distribute::NewBackendCache<Concrete<T>, (), N, S>;

// === impl Router ===

impl<T, M, F> Router<T, M, F>
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
    route::backend::Matched<T, M::Summary, F>: route::filters::Apply,
{
    /// Wraps a `NewService`--instantiated once per logical target--that caches a set
    /// of concrete services so that, as the watch provides new `Params`, we can
    /// reuse inner services.
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

impl<T, M, F> From<(Params<M, F>, T)> for Router<T, M, F>
where
    T: Eq + Hash + Clone + Debug,
    M: Clone,
    F: Clone,
{
    fn from((rts, parent): (Params<M, F>, T)) -> Self {
        let Params {
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
                http::balance::EwmaConfig { decay, default_rtt },
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
            policy::RouteDistribution::Empty => route::BackendDistribution::Empty,
            policy::RouteDistribution::FirstAvailable(backends) => {
                route::BackendDistribution::first_available(backends.iter().map(mk_route_backend))
            }
            policy::RouteDistribution::RandomAvailable(backends) => {
                route::BackendDistribution::random_available(
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

impl<B, T> svc::router::SelectRoute<http::Request<B>> for Http<T>
where
    T: Eq + Hash + Clone + Debug,
{
    type Key = route::Http<T>;
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

impl<T, B> svc::router::SelectRoute<http::Request<B>> for Grpc<T>
where
    T: Eq + Hash + Clone + Debug,
{
    type Key = route::Grpc<T>;
    type Error = NoRoute;

    fn select(&self, req: &http::Request<B>) -> Result<Self::Key, Self::Error> {
        let (r#match, params) = policy::grpc::find(&*self.routes, req).ok_or(NoRoute)?;
        Ok(route::Matched {
            r#match,
            params: params.clone(),
        })
    }
}

impl<T, M, F> svc::Param<LogicalAddr> for Router<T, M, F>
where
    T: Eq + Hash + Clone + Debug,
{
    fn param(&self) -> LogicalAddr {
        LogicalAddr(self.addr.clone())
    }
}

impl<T, M, F> svc::Param<distribute::Backends<Concrete<T>>> for Router<T, M, F>
where
    T: Eq + Hash + Clone + Debug,
{
    fn param(&self) -> distribute::Backends<Concrete<T>> {
        self.backends.clone()
    }
}
