use super::super::Concrete;
use crate::{ParentRef, RouteRef};
use linkerd_app_core::{classify, proxy::http, svc, Addr, Error, Result};
use linkerd_distribute as distribute;
use linkerd_http_route as http_route;
use linkerd_proxy_client_policy as policy;
use std::{fmt::Debug, hash::Hash, sync::Arc};

pub(crate) mod backend;
pub(crate) mod filters;
pub(crate) mod metrics;

pub(crate) use self::backend::{Backend, MatchedBackend};
pub use self::filters::errors;
use self::metrics::labels::Route as RouteLabels;

pub use self::metrics::{GrpcRouteMetrics, HttpRouteMetrics};

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
    pub(super) parent_ref: ParentRef,
    pub(super) route_ref: RouteRef,
    pub(super) filters: Arc<[F]>,
    pub(super) distribution: BackendDistribution<T, F>,
    pub(super) failure_policy: E,
    pub(super) request_timeout: Option<std::time::Duration>,
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

pub type Metrics<R, B> = metrics::RouteMetrics<
    <R as metrics::MkStreamLabel>::StreamLabel,
    <B as metrics::MkStreamLabel>::StreamLabel,
>;

/// Wraps errors with route metadata.
#[derive(Debug, thiserror::Error)]
#[error("route {}: {source}", route.0)]
struct RouteError {
    route: RouteRef,
    #[source]
    source: Error,
}

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
    Self: metrics::MkStreamLabel,
    MatchedBackend<T, M, F>: filters::Apply,
    MatchedBackend<T, M, F>: metrics::MkStreamLabel,
{
    /// Builds a route stack that applies policy filters to requests and
    /// distributes requests over each route's backends. These [`Concrete`]
    /// backends are expected to be cached/shared by the inner stack.
    pub(crate) fn layer<N, S>(
        metrics: Metrics<Self, MatchedBackend<T, M, F>>,
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
                // Distribute requests across route backends, applying policies
                // and filters for each of the route-backends.
                .push(MatchedBackend::layer(metrics.backend.clone()))
                .lift_new_with_target()
                .push(NewDistribute::layer())
                // The router does not take the backend's availability into
                // consideration, so we must eagerly fail requests to prevent
                // leaking tasks onto the runtime.
                .push_on_service(svc::LoadShed::layer())
                .push(filters::NewApplyFilters::<Self, _, _>::layer())
                .push(http::NewTimeout::layer())
                .push(metrics::layer(&metrics.requests))
                // Configure a classifier to use in the endpoint stack.
                // FIXME(ver) move this into NewSetExtensions
                .push(classify::NewClassify::layer())
                .push(svc::NewMapErr::layer_with(|rt: &Self| {
                    let route = rt.params.route_ref.clone();
                    move |source| RouteError {
                        route: route.clone(),
                        source,
                    }
                }))
                .arc_new_clone_http()
                .into_inner()
        })
    }
}

impl<T: Clone, M, F, P> svc::Param<BackendDistribution<T, F>> for MatchedRoute<T, M, F, P> {
    fn param(&self) -> BackendDistribution<T, F> {
        self.params.distribution.clone()
    }
}

impl<T: Clone, M, F, P> svc::Param<RouteLabels> for MatchedRoute<T, M, F, P> {
    fn param(&self) -> RouteLabels {
        RouteLabels(
            self.params.parent_ref.clone(),
            self.params.route_ref.clone(),
        )
    }
}

impl<T, M, F, P> svc::Param<http::timeout::ResponseTimeout> for MatchedRoute<T, M, F, P> {
    fn param(&self) -> http::timeout::ResponseTimeout {
        http::timeout::ResponseTimeout(self.params.request_timeout)
    }
}

// === impl Http ===

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
    type StatusLabels = metrics::labels::HttpRouteRsp;
    type DurationLabels = metrics::labels::Route;
    type StreamLabel = metrics::LabelHttpRouteRsp;

    fn mk_stream_labeler<B>(&self, _: &::http::Request<B>) -> Option<Self::StreamLabel> {
        let parent = self.params.parent_ref.clone();
        let route = self.params.route_ref.clone();
        Some(metrics::LabelHttpRsp::from(metrics::labels::Route::from((
            parent, route,
        ))))
    }
}

impl<T> svc::Param<classify::Request> for Http<T> {
    fn param(&self) -> classify::Request {
        classify::Request::ClientPolicy(classify::ClientPolicy::Http(
            policy::http::StatusRanges::default(),
        ))
    }
}

// === impl Grpc ===

impl<T> filters::Apply for Grpc<T> {
    #[inline]
    fn apply_request<B>(&self, req: &mut ::http::Request<B>) -> Result<()> {
        filters::apply_grpc_request(&self.r#match, &self.params.filters, req)
    }

    #[inline]
    fn apply_response<B>(&self, rsp: &mut ::http::Response<B>) -> Result<()> {
        filters::apply_grpc_response(&self.params.filters, rsp)
    }
}

impl<T> metrics::MkStreamLabel for Grpc<T> {
    type StatusLabels = metrics::labels::GrpcRouteRsp;
    type DurationLabels = metrics::labels::Route;
    type StreamLabel = metrics::LabelGrpcRouteRsp;

    fn mk_stream_labeler<B>(&self, _: &::http::Request<B>) -> Option<Self::StreamLabel> {
        let parent = self.params.parent_ref.clone();
        let route = self.params.route_ref.clone();
        Some(metrics::LabelGrpcRsp::from(metrics::labels::Route::from((
            parent, route,
        ))))
    }
}

impl<T> svc::Param<classify::Request> for Grpc<T> {
    fn param(&self) -> classify::Request {
        classify::Request::ClientPolicy(
            classify::ClientPolicy::Grpc(policy::grpc::Codes::default()),
        )
    }
}
