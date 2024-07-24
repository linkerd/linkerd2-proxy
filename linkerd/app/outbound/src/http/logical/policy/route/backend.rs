use super::{super::Concrete, filters};
use crate::{BackendRef, ParentRef, RouteRef};
use linkerd_app_core::{proxy::http, svc, Error, Result};
use linkerd_http_prom::record_response::MkStreamLabel;
use linkerd_http_route as http_route;
use linkerd_proxy_client_policy as policy;
use std::{fmt::Debug, hash::Hash, sync::Arc};

pub(super) mod metrics;

#[derive(Debug, PartialEq, Eq, Hash)]
pub(crate) struct Backend<T, F> {
    pub(crate) route_ref: RouteRef,
    pub(crate) concrete: Concrete<T>,
    pub(crate) filters: Arc<[F]>,
}

pub(crate) type MatchedBackend<T, M, F> = super::Matched<M, Backend<T, F>>;
pub(crate) type Http<T> =
    MatchedBackend<T, http_route::http::r#match::RequestMatch, policy::http::Filter>;
pub(crate) type Grpc<T> =
    MatchedBackend<T, http_route::grpc::r#match::RouteMatch, policy::grpc::Filter>;

pub type Metrics<T> = metrics::RouteBackendMetrics<<T as MkStreamLabel>::StreamLabel>;

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
        }
    }
}

// === impl Matched ===

impl<M, T, F, P> From<(Backend<T, F>, super::MatchedRoute<T, M, F, P>)>
    for MatchedBackend<T, M, F>
{
    fn from((params, route): (Backend<T, F>, super::MatchedRoute<T, M, F, P>)) -> Self {
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
    Self: metrics::MkStreamLabel,
{
    /// Builds a stack that applies per-route-backend policy filters over an
    /// inner [`Concrete`] stack.
    ///
    /// This [`MatchedBackend`] must implement [`filters::Apply`] to apply these
    /// filters.
    pub(crate) fn layer<N, S>(
        metrics: Metrics<Self>,
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
                .push(metrics::layer(&metrics))
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

impl<T> metrics::MkStreamLabel for Http<T> {
    type StatusLabels = metrics::labels::HttpRouteBackendRsp;
    type DurationLabels = metrics::labels::RouteBackend;
    type StreamLabel = metrics::LabelHttpRouteBackendRsp;

    fn mk_stream_labeler<B>(&self, _: &::http::Request<B>) -> Option<Self::StreamLabel> {
        let parent = self.params.concrete.parent_ref.clone();
        let route = self.params.route_ref.clone();
        let backend = self.params.concrete.backend_ref.clone();
        Some(metrics::LabelHttpRsp::from(
            metrics::labels::RouteBackend::from((parent, route, backend)),
        ))
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

impl<T> metrics::MkStreamLabel for Grpc<T> {
    type StatusLabels = metrics::labels::GrpcRouteBackendRsp;
    type DurationLabels = metrics::labels::RouteBackend;
    type StreamLabel = metrics::LabelGrpcRouteBackendRsp;

    fn mk_stream_labeler<B>(&self, _: &::http::Request<B>) -> Option<Self::StreamLabel> {
        let parent = self.params.concrete.parent_ref.clone();
        let route = self.params.route_ref.clone();
        let backend = self.params.concrete.backend_ref.clone();
        Some(metrics::LabelGrpcRsp::from(
            metrics::labels::RouteBackend::from((parent, route, backend)),
        ))
    }
}
