use super::{super::Concrete, filters};
use crate::{BackendRef, ParentRef, RouteRef};
use linkerd_app_core::{proxy::http, svc, Error, Result};
use linkerd_http_route as http_route;
use linkerd_proxy_client_policy as policy;
use std::{fmt::Debug, hash::Hash, sync::Arc};

mod count_reqs;
mod metrics;

pub use self::count_reqs::RequestCount;
pub use self::metrics::RouteBackendMetrics;

#[derive(Debug, PartialEq, Eq, Hash)]
pub(crate) struct Backend<T, F> {
    pub(crate) route_ref: RouteRef,
    pub(crate) concrete: Concrete<T>,
    pub(crate) filters: Arc<[F]>,
    pub(crate) request_timeout: Option<std::time::Duration>,
}

pub(crate) type MatchedBackend<T, M, F> = super::Matched<M, Backend<T, F>>;
pub(crate) type Http<T> =
    MatchedBackend<T, http_route::http::r#match::RequestMatch, policy::http::Filter>;
pub(crate) type Grpc<T> =
    MatchedBackend<T, http_route::grpc::r#match::RouteMatch, policy::grpc::Filter>;

/// Wraps errors with backend metadata.
#[derive(Debug, thiserror::Error)]
#[error("backend {}: {source}", backend.0)]
struct BackendError {
    backend: BackendRef,
    #[source]
    source: Error,
}

// === impl Backend ===

impl<T: Clone, F> Clone for Backend<T, F> {
    fn clone(&self) -> Self {
        Self {
            route_ref: self.route_ref.clone(),
            filters: self.filters.clone(),
            concrete: self.concrete.clone(),
            request_timeout: self.request_timeout,
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
    RouteBackendMetrics: svc::ExtractParam<RequestCount, Self>,
{
    /// Builds a stack that applies per-route-backend policy filters over an
    /// inner [`Concrete`] stack.
    ///
    /// This [`MatchedBackend`] must implement [`filters::Apply`] to apply these
    /// filters.
    pub(crate) fn layer<N, S>(
        metrics: RouteBackendMetrics,
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
                .push_map_target(
                    |Self {
                         params: Backend { concrete, .. },
                         ..
                     }| concrete,
                )
                .push(filters::NewApplyFilters::<Self, _, _>::layer())
                .push(http::NewTimeout::layer())
                .push(count_reqs::NewCountRequests::layer_via(metrics.clone()))
                .push(svc::NewMapErr::layer_with(|t: &Self| {
                    let backend = t.params.concrete.backend_ref.clone();
                    move |source| {
                        Error::from(BackendError {
                            backend: backend.clone(),
                            source,
                        })
                    }
                }))
                .arc_new_clone_http()
                .into_inner()
        })
    }
}

impl<T, M, F> svc::Param<http::ResponseTimeout> for MatchedBackend<T, M, F> {
    fn param(&self) -> http::ResponseTimeout {
        http::ResponseTimeout(self.params.request_timeout)
    }
}

impl<T, M, F> svc::Param<ParentRef> for MatchedBackend<T, M, F> {
    fn param(&self) -> ParentRef {
        self.params.concrete.parent_ref.clone()
    }
}

impl<T, M, F> svc::Param<RouteRef> for MatchedBackend<T, M, F> {
    fn param(&self) -> RouteRef {
        self.params.route_ref.clone()
    }
}

impl<T, M, F> svc::Param<BackendRef> for MatchedBackend<T, M, F> {
    fn param(&self) -> BackendRef {
        self.params.concrete.backend_ref.clone()
    }
}

impl<T> filters::Apply for Http<T> {
    #[inline]
    fn apply_request<B>(&self, req: &mut ::http::Request<B>) -> Result<()> {
        filters::apply_http_request(&self.r#match, &self.params.filters, req)
    }

    #[inline]
    fn apply_response<B>(&self, rsp: &mut ::http::Response<B>) -> Result<()> {
        filters::apply_http_response(&self.params.filters, rsp)
    }
}

impl<T> filters::Apply for Grpc<T> {
    #[inline]
    fn apply_request<B>(&self, req: &mut ::http::Request<B>) -> Result<()> {
        filters::apply_grpc_request(&self.r#match, &self.params.filters, req)
    }

    fn apply_response<B>(&self, rsp: &mut ::http::Response<B>) -> Result<()> {
        filters::apply_grpc_response(&self.params.filters, rsp)
    }
}
