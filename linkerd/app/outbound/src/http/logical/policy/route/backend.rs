use super::{super::Concrete, filters};
use linkerd_app_core::{proxy::http, svc, Error, Result};
use linkerd_http_route as http_route;
use linkerd_proxy_client_policy as policy;
use std::{fmt::Debug, hash::Hash, sync::Arc};

mod count_reqs;
mod metrics;

pub use self::{
    count_reqs::RequestCount,
    metrics::{RouteBackendMeta, RouteBackendMetrics},
};

#[derive(Debug, PartialEq, Eq, Hash)]
pub(crate) struct Backend<T, F> {
    pub(crate) meta: RouteBackendMeta,
    pub(crate) concrete: Concrete<T>,
    pub(crate) filters: Arc<[F]>,
}

pub(crate) type MatchedBackend<T, M, F> = super::Matched<M, Backend<T, F>>;
pub(crate) type Http<T> =
    MatchedBackend<T, http_route::http::r#match::RequestMatch, policy::http::Filter>;
pub(crate) type Grpc<T> =
    MatchedBackend<T, http_route::grpc::r#match::RouteMatch, policy::grpc::Filter>;

#[derive(Clone, Debug)]
pub struct ExtractMetrics {
    metrics: RouteBackendMetrics,
}

// === impl Backend ===

impl<T: Clone, F> Clone for Backend<T, F> {
    fn clone(&self) -> Self {
        Self {
            meta: self.meta.clone(),
            filters: self.filters.clone(),
            concrete: self.concrete.clone(),
        }
    }
}

// === impl Matched ===

impl<M, T, F, E> From<(Backend<T, F>, super::MatchedRoute<T, M, F, E>)>
    for MatchedBackend<T, M, F>
{
    fn from((params, route): (Backend<T, F>, super::MatchedRoute<T, M, F, E>)) -> Self {
        MatchedBackend {
            r#match: route.r#match,
            params,
        }
    }
}

impl<T, M, F> MatchedBackend<T, M, F>
where
    // Parent target.
    T: Debug + Eq + Hash,
    T: Clone + Send + Sync + 'static,
    // Request match summary
    M: Clone + Send + Sync + 'static,
    // Request filter.
    F: Clone + Send + Sync + 'static,
    // Assert that filters can be applied.
    Self: filters::Apply,
    ExtractMetrics: svc::ExtractParam<RequestCount, Self>,
{
    /// Builds a stack that applies per-route-backend policy filters over an
    /// inner [`Concrete`] stack.
    ///
    /// This [`MatchedBackend`] must implement [`filters::Apply`] to apply these
    /// filters.
    pub(crate) fn layer<N, S>(
        metrics: RouteBackendMetrics,
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
                .push_map_target(
                    |Self {
                         params: Backend { concrete, .. },
                         ..
                     }| concrete,
                )
                .push(filters::NewApplyFilters::<Self, _, _>::layer())
                .push(count_reqs::NewCountRequests::layer_via(ExtractMetrics {
                    metrics: metrics.clone(),
                }))
                .push(svc::ArcNewService::layer())
                .into_inner()
        })
    }
}

impl<T> filters::Apply for Http<T> {
    #[inline]
    fn apply<B>(&self, req: &mut ::http::Request<B>) -> Result<()> {
        filters::apply_http(&self.r#match, &self.params.filters, req)
    }
}

impl<T> filters::Apply for Grpc<T> {
    #[inline]
    fn apply<B>(&self, req: &mut ::http::Request<B>) -> Result<()> {
        filters::apply_grpc(&self.r#match, &self.params.filters, req)
    }
}

impl<T> svc::ExtractParam<RequestCount, Http<T>> for ExtractMetrics {
    fn extract_param(&self, params: &Http<T>) -> RequestCount {
        RequestCount(self.metrics.http_requests_total(params.params.meta.clone()))
    }
}

impl<T> svc::ExtractParam<RequestCount, Grpc<T>> for ExtractMetrics {
    fn extract_param(&self, params: &Grpc<T>) -> RequestCount {
        RequestCount(self.metrics.grpc_requests_total(params.params.meta.clone()))
    }
}
