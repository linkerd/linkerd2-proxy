use super::{
    super::{concrete, Concrete, LogicalAddr, NoRoute},
    route,
};
use crate::{BackendRef, EndpointRef, ParentRef, RouteRef};
use linkerd_app_core::{
    classify, proxy::http, svc, transport::addrs::*, Addr, Error, NameAddr, Result,
};
use linkerd_distribute as distribute;
use linkerd_http_prom as http_prom;
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
    Params<http_route::http::MatchRequest, policy::http::Filter, policy::http::RouteParams>;
pub type GrpcParams =
    Params<http_route::grpc::MatchRoute, policy::grpc::Filter, policy::grpc::RouteParams>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Router<T: Clone + Debug + Eq + Hash, M, F, E> {
    pub(super) parent: T,
    pub(super) addr: Addr,
    pub(super) routes: Arc<[http_route::Route<M, route::Route<T, F, E>>]>,
    pub(super) backends: distribute::Backends<Concrete<T>>,
}

pub(super) type Http<T> =
    Router<T, http_route::http::MatchRequest, policy::http::Filter, policy::http::RouteParams>;
pub(super) type Grpc<T> =
    Router<T, http_route::grpc::MatchRoute, policy::grpc::Filter, policy::grpc::RouteParams>;

type NewBackendCache<T, N, S> = distribute::NewBackendCache<Concrete<T>, (), N, S>;

// === impl Router ===

impl<T, M, F, P> Router<T, M, F, P>
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
    // Route policy.
    P: Debug + Eq + Hash,
    P: Clone + Send + Sync + 'static,
    // Assert that we can route for the given match and filter types.
    Self: svc::router::SelectRoute<
        http::Request<http::BoxBody>,
        Key = route::MatchedRoute<T, M::Summary, F, P>,
        Error = NoRoute,
    >,
    route::MatchedRoute<T, M::Summary, F, P>: route::filters::Apply
        + svc::Param<classify::Request>
        + svc::Param<route::extensions::Params>
        + http_prom::MkStreamLabel,
    route::MatchedBackend<T, M::Summary, F>: route::filters::Apply + http_prom::MkStreamLabel,
{
    /// Builds a stack that applies routes to distribute requests over a cached
    /// set of inner services so that.
    pub(super) fn layer<N, S>(
        metrics: route::Metrics<
            route::MatchedRoute<T, M::Summary, F, P>,
            route::MatchedBackend<T, M::Summary, F>,
        >,
    ) -> impl svc::Layer<N, Service = svc::ArcNewCloneHttp<Self>> + Clone
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
                .push_on_service(route::MatchedRoute::layer(metrics.clone()))
                .push(svc::NewOneshotRoute::<Self, (), _>::layer_cached())
                .arc_new_clone_http()
                .into_inner()
        })
    }
}

impl<T, M, F, P> From<(Params<M, F, P>, T)> for Router<T, M, F, P>
where
    T: Eq + Hash + Clone + Debug,
    M: Clone,
    F: Clone,
    P: Clone,
{
    fn from((rts, parent): (Params<M, F, P>, T)) -> Self {
        let Params {
            addr,
            meta: parent_ref,
            routes,
            backends,
            failure_accrual,
        } = rts;

        let mk_concrete = {
            let parent = parent.clone();
            let parent_ref = parent_ref.clone();
            move |backend_ref: BackendRef, target: concrete::Dispatch| {
                // XXX With policies we don't have a top-level authority name at
                // the moment. So, instead, we use the concrete addr used for
                // discovery for now.
                let authority = match target {
                    concrete::Dispatch::Balance(ref addr, ..) => Some(addr.as_http_authority()),
                    _ => None,
                };
                Concrete {
                    target,
                    authority,
                    parent: parent.clone(),
                    backend_ref,
                    parent_ref: parent_ref.clone(),
                    failure_accrual,
                }
            }
        };

        let mk_dispatch = move |bke: &policy::Backend| match bke.dispatcher {
            policy::BackendDispatcher::BalanceP2c(
                policy::Load::PeakEwma(policy::PeakEwma { decay, default_rtt }),
                policy::EndpointDiscovery::DestinationGet { ref path },
            ) => mk_concrete(
                BackendRef(bke.meta.clone()),
                concrete::Dispatch::Balance(
                    path.parse::<NameAddr>()
                        .expect("destination must be a nameaddr"),
                    http::balance::EwmaConfig { decay, default_rtt },
                ),
            ),
            policy::BackendDispatcher::Forward(addr, ref md) => mk_concrete(
                EndpointRef::new(md, addr.port().try_into().expect("port must not be 0")).into(),
                concrete::Dispatch::Forward(Remote(ServerAddr(addr)), md.clone()),
            ),
            policy::BackendDispatcher::Fail { ref message } => mk_concrete(
                BackendRef(policy::Meta::new_default("fail")),
                concrete::Dispatch::Fail {
                    message: message.clone(),
                },
            ),
        };

        let mk_route_backend = |route_ref: &RouteRef, rb: &policy::RouteBackend<F>| {
            let filters = rb.filters.clone();
            let concrete = mk_dispatch(&rb.backend);
            route::Backend {
                route_ref: route_ref.clone(),
                filters,
                concrete,
            }
        };

        let mk_distribution = |rr: &RouteRef, d: &policy::RouteDistribution<F>| match d {
            policy::RouteDistribution::Empty => route::BackendDistribution::Empty,
            policy::RouteDistribution::FirstAvailable(backends) => {
                route::BackendDistribution::first_available(
                    backends.iter().map(|b| mk_route_backend(rr, b)),
                )
            }
            policy::RouteDistribution::RandomAvailable(backends) => {
                route::BackendDistribution::random_available(
                    backends
                        .iter()
                        .map(|(rb, weight)| (mk_route_backend(rr, rb), *weight)),
                )
                .expect("distribution must be valid")
            }
        };

        let mk_policy = {
            let addr = addr.clone();
            let parent = parent.clone();
            let parent_ref = parent_ref.clone();
            move |policy::RoutePolicy::<F, P> {
                      meta,
                      filters,
                      distribution,
                      params,
                  }| {
                let route_ref = RouteRef(meta);
                let distribution = mk_distribution(&route_ref, &distribution);
                route::Route {
                    addr: addr.clone(),
                    parent: parent.clone(),
                    parent_ref: parent_ref.clone(),
                    route_ref,
                    filters,
                    distribution,
                    params,
                }
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
        let (r#match, params) = policy::http::find(&self.routes, req).ok_or(NoRoute)?;
        tracing::debug!(meta = ?params.route_ref, "Selected route");
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
        let (r#match, params) = policy::grpc::find(&self.routes, req).ok_or(NoRoute)?;
        tracing::debug!(meta = ?params.route_ref, "Selected route");
        tracing::trace!(?r#match);
        Ok(route::Matched {
            r#match,
            params: params.clone(),
        })
    }
}

impl<T, M, F, P> svc::Param<LogicalAddr> for Router<T, M, F, P>
where
    T: Eq + Hash + Clone + Debug,
{
    fn param(&self) -> LogicalAddr {
        LogicalAddr(self.addr.clone())
    }
}

impl<T, M, F, P> svc::Param<distribute::Backends<Concrete<T>>> for Router<T, M, F, P>
where
    T: Eq + Hash + Clone + Debug,
{
    fn param(&self) -> distribute::Backends<Concrete<T>> {
        self.backends.clone()
    }
}
