use super::{
    super::{concrete, Concrete, LogicalAddr, NoRoute},
    route::{self},
};
use crate::{BackendRef, ParentRef};
use linkerd_app_core::{
    classify, proxy::http, svc, transport::addrs::*, Addr, Error, NameAddr, Result,
};
use linkerd_distribute as distribute;
use linkerd_http_route as http_route;
use linkerd_proxy_client_policy as policy;
use std::{fmt::Debug, hash::Hash, sync::Arc};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Params<M, F, E> {
    pub addr: Addr,
    pub meta: ParentRef,
    pub routes: Arc<[http_route::Route<M, policy::RoutePolicy<F, E>>]>,
    pub backends: Arc<[policy::Backend]>,
    pub failure_accrual: policy::FailureAccrual,
}

pub type HttpParams =
    Params<http_route::http::MatchRequest, policy::http::Filter, policy::http::StatusRanges>;
pub type GrpcParams =
    Params<http_route::grpc::MatchRoute, policy::grpc::Filter, policy::grpc::Codes>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Router<T: Clone + Debug + Eq + Hash, M, F, E> {
    pub(super) parent: T,
    pub(super) addr: Addr,
    pub(super) routes: Arc<[http_route::Route<M, route::Route<T, F, E>>]>,
    pub(super) backends: distribute::Backends<Concrete<T>>,
}

pub(super) type Http<T> =
    Router<T, http_route::http::MatchRequest, policy::http::Filter, policy::http::StatusRanges>;
pub(super) type Grpc<T> =
    Router<T, http_route::grpc::MatchRoute, policy::grpc::Filter, policy::grpc::Codes>;

type NewBackendCache<T, N, S> = distribute::NewBackendCache<Concrete<T>, (), N, S>;

// === impl Router ===

impl<T, M, F, E> Router<T, M, F, E>
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
    // Failure policy.
    E: Debug + Eq + Hash,
    E: Clone + Send + Sync + 'static,
    // Assert that we can route for the given match and filter types.
    Self: svc::router::SelectRoute<
        http::Request<http::BoxBody>,
        Key = route::MatchedRoute<T, M::Summary, F, E>,
        Error = NoRoute,
    >,
    route::MatchedRoute<T, M::Summary, F, E>: route::filters::Apply + svc::Param<classify::Request>,
    route::MatchedBackend<T, M::Summary, F>: route::filters::Apply,
{
    /// Builds a stack that applies routes to distribute requests over a cached
    /// set of inner services so that.
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
        svc::layer::mk(move |inner| {
            svc::stack(inner)
                .lift_new()
                // Each route builds over concrete backends. All of these
                // backends are cached here and shared across routes.
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

impl<T, M, F, E> From<(Params<M, F, E>, T)> for Router<T, M, F, E>
where
    T: Eq + Hash + Clone + Debug,
    M: Clone,
    F: Clone,
    E: Clone,
{
    fn from((rts, parent): (Params<M, F, E>, T)) -> Self {
        let Params {
            addr,
            meta: parent_ref,
            routes,
            backends,
            failure_accrual,
        } = rts;

        let mk_concrete = {
            let parent = parent.clone();
            move |target: concrete::Dispatch| {
                // XXX With policies we don't have a top-level authority name at
                // the moment. So, instead, we use the concrete addr used for
                // discovery for now.
                let authority = match target {
                    concrete::Dispatch::Balance { ref addr, .. } => Some(addr.as_http_authority()),
                    _ => None,
                };
                Concrete {
                    target,
                    authority,
                    parent: parent.clone(),
                    parent_ref: parent_ref.clone(),
                    failure_accrual,
                }
            }
        };

        let mk_dispatch = move |bke: &policy::Backend| match bke.dispatcher {
            policy::BackendDispatcher::BalanceP2c(
                policy::Load::PeakEwma(policy::PeakEwma { decay, default_rtt }),
                policy::EndpointDiscovery::DestinationGet { ref path },
            ) => mk_concrete(concrete::Dispatch::Balance {
                addr: path
                    .parse::<NameAddr>()
                    .expect("destination must be a nameaddr"),
                meta: BackendRef(bke.meta.clone()),
                ewma: http::balance::EwmaConfig { decay, default_rtt },
            }),
            policy::BackendDispatcher::Forward(addr, ref metadata) => mk_concrete(
                concrete::Dispatch::Forward(Remote(ServerAddr(addr)), metadata.clone()),
            ),
            policy::BackendDispatcher::Fail { ref message } => {
                mk_concrete(concrete::Dispatch::Fail {
                    message: message.clone(),
                })
            }
        };

        let mk_route_backend = {
            let mk_dispatch = mk_dispatch.clone();
            move |rb: &policy::RouteBackend<F>| {
                let filters = rb.filters.clone();
                let concrete = mk_dispatch(&rb.backend);
                route::Backend { filters, concrete }
            }
        };

        let mk_distribution = |d: &policy::RouteDistribution<F>| match d {
            policy::RouteDistribution::Empty => route::BackendDistribution::Empty,
            policy::RouteDistribution::FirstAvailable(backends) => {
                route::BackendDistribution::first_available(
                    backends.iter().map(|b| mk_route_backend(b)),
                )
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

        let mk_policy = |policy::RoutePolicy::<F, E> {
                             meta,
                             filters,
                             distribution,
                             failure_policy,
                         }| {
            let distribution = mk_distribution(&distribution);
            route::Route {
                addr: addr.clone(),
                parent: parent.clone(),
                meta,
                filters,
                failure_policy,
                distribution,
            }
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

        let backends = backends.iter().map(mk_dispatch).collect();

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
        tracing::trace!(uri = ?req.uri(), headers = ?req.headers(), "Selecting HTTP route");
        let (r#match, params) = policy::http::find(&*self.routes, req).ok_or(NoRoute)?;
        tracing::debug!(meta = ?params.meta, "Selected route");
        tracing::trace!(?r#match);
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
        tracing::trace!(uri = ?req.uri(), headers = ?req.headers(), "Selecting gRPC route");
        let (r#match, params) = policy::grpc::find(&*self.routes, req).ok_or(NoRoute)?;
        tracing::debug!(meta = ?params.meta, "Selected route");
        tracing::trace!(?r#match);
        Ok(route::Matched {
            r#match,
            params: params.clone(),
        })
    }
}

impl<T, M, F, E> svc::Param<LogicalAddr> for Router<T, M, F, E>
where
    T: Eq + Hash + Clone + Debug,
{
    fn param(&self) -> LogicalAddr {
        LogicalAddr(self.addr.clone())
    }
}

impl<T, M, F, E> svc::Param<distribute::Backends<Concrete<T>>> for Router<T, M, F, E>
where
    T: Eq + Hash + Clone + Debug,
{
    fn param(&self) -> distribute::Backends<Concrete<T>> {
        self.backends.clone()
    }
}
