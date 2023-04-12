use super::super::Concrete;
use crate::RouteRef;
use linkerd_app_core::{classify, proxy::http, svc, Addr, Error, Result};
use linkerd_distribute as distribute;
use linkerd_http_route as http_route;
use linkerd_proxy_client_policy as policy;
use std::{fmt::Debug, hash::Hash, sync::Arc};

pub(crate) mod backend;
pub(crate) mod filters;

pub(crate) use self::backend::{Backend, MatchedBackend};
pub use self::filters::errors;

/// A target type that includes a summary of exactly how a request was matched.
/// This match state is required to apply route filters.
///
/// See [`MatchedRoute`] and [`MatchedBackend`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Matched<M, P> {
    pub(super) r#match: http_route::RouteMatch<M>,
    pub(super) params: P,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Route<T, F, E> {
    pub(super) parent: T,
    pub(super) addr: Addr,
    pub(super) route_ref: RouteRef,
    pub(super) filters: Arc<[F]>,
    pub(super) distribution: BackendDistribution<T, F>,
    pub(super) failure_policy: E,
}

pub(crate) type MatchedRoute<T, M, F, E> = Matched<M, Route<T, F, E>>;
pub(crate) type Http<T> = MatchedRoute<
    T,
    http_route::http::r#match::RequestMatch,
    policy::http::Filter,
    policy::http::StatusRanges,
>;
pub(crate) type Grpc<T> = MatchedRoute<
    T,
    http_route::grpc::r#match::RouteMatch,
    policy::grpc::Filter,
    policy::grpc::Codes,
>;

pub(crate) type BackendDistribution<T, F> = distribute::Distribution<Backend<T, F>>;
pub(crate) type NewDistribute<T, F, N> = distribute::NewDistribute<Backend<T, F>, (), N>;

// === impl MatchedRoute ===

impl<T, M, F, E> MatchedRoute<T, M, F, E>
where
    // Parent target.
    T: Debug + Eq + Hash,
    T: Clone + Send + Sync + 'static,
    // Match summary
    M: Clone + Send + Sync + 'static,
    // Request filter.
    F: Debug + Eq + Hash,
    F: Clone + Send + Sync + 'static,
    // Failure policy.
    E: Clone + Send + Sync + 'static,
    // Assert that filters can be applied.
    Self: filters::Apply,
    Self: svc::Param<classify::Request>,
    MatchedBackend<T, M, F>: filters::Apply,
    backend::ExtractMetrics: svc::ExtractParam<backend::RequestCount, MatchedBackend<T, M, F>>,
{
    /// Builds a route stack that applies policy filters to requests and
    /// distributes requests over each route's backends. These [`Concrete`]
    /// backends are expected to be cached/shared by the inner stack.
    pub(crate) fn layer<N, S>(
        backend_metrics: backend::RouteBackendMetrics,
    ) -> impl svc::Layer<
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
                // Distribute requests across route backends, applying policies
                // and filters for each of the route-backends.
                .push(MatchedBackend::layer(backend_metrics.clone()))
                .lift_new_with_target()
                .push(NewDistribute::layer())
                // The router does not take the backend's availability into
                // consideration, so we must eagerly fail requests to prevent
                // leaking tasks onto the runtime.
                .push_on_service(svc::LoadShed::layer())
                // TODO(ver) attach the `E` typed failure policy to requests.
                .push(filters::NewApplyFilters::<Self, _, _>::layer())
                .push(classify::NewClassify::layer())
                .push(svc::ArcNewService::layer())
                .into_inner()
        })
    }
}

impl<T: Clone, M, F, E> svc::Param<BackendDistribution<T, F>> for MatchedRoute<T, M, F, E> {
    fn param(&self) -> BackendDistribution<T, F> {
        self.params.distribution.clone()
    }
}

impl<T> filters::Apply for Http<T> {
    #[inline]
    fn apply<B>(&self, req: &mut ::http::Request<B>) -> Result<()> {
        filters::apply_http(&self.r#match, &self.params.filters, req)
    }
}

impl<T> svc::Param<classify::Request> for Http<T> {
    fn param(&self) -> classify::Request {
        classify::Request::ClientPolicy(classify::ClientPolicy::Http(
            self.params.failure_policy.clone(),
        ))
    }
}

impl<T> filters::Apply for Grpc<T> {
    #[inline]
    fn apply<B>(&self, req: &mut ::http::Request<B>) -> Result<()> {
        filters::apply_grpc(&self.r#match, &self.params.filters, req)
    }
}

impl<T> svc::Param<classify::Request> for Grpc<T> {
    fn param(&self) -> classify::Request {
        classify::Request::ClientPolicy(classify::ClientPolicy::Grpc(
            self.params.failure_policy.clone(),
        ))
    }
}
