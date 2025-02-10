use super::super::Concrete;
use crate::{ParentRef, RouteRef};
use linkerd_app_core::{classify, proxy::http, svc, Addr, Error, Result};
use linkerd_distribute as distribute;
use linkerd_http_route as http_route;
use linkerd_proxy_client_policy as policy;
use std::{fmt::Debug, hash::Hash, sync::Arc};

pub(crate) mod backend;
pub(crate) mod extensions;
pub(crate) mod filters;
pub(crate) mod metrics;
pub(crate) mod retry;

pub(crate) use self::backend::{Backend, MatchedBackend};
pub use self::filters::errors;

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
pub(crate) struct Route<T, F, P> {
    pub(super) parent: T,
    pub(super) addr: Addr,
    pub(super) parent_ref: ParentRef,
    pub(super) route_ref: RouteRef,
    pub(super) filters: Arc<[F]>,
    pub(super) distribution: BackendDistribution<T, F>,
    pub(super) params: P,
}

pub(crate) type MatchedRoute<T, M, F, P> = Matched<M, Route<T, F, P>>;
pub(crate) type Http<T> = MatchedRoute<
    T,
    http_route::http::r#match::RequestMatch,
    policy::http::Filter,
    policy::http::RouteParams,
>;
pub(crate) type Grpc<T> = MatchedRoute<
    T,
    http_route::grpc::r#match::RouteMatch,
    policy::grpc::Filter,
    policy::grpc::RouteParams,
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

impl<T, M, F, P> MatchedRoute<T, M, F, P>
where
    // Parent target.
    T: Debug + Eq + Hash,
    T: Clone + Send + Sync + 'static,
    // Match summary
    M: Clone + Send + Sync + 'static,
    // Request filter.
    F: Debug + Eq + Hash,
    F: Clone + Send + Sync + 'static,
    // Route params.
    P: Clone + Send + Sync + 'static,
    // Assert that filters can be applied.
    Self: filters::Apply,
    Self: svc::Param<classify::Request>,
    Self: svc::Param<extensions::Params>,
    Self: metrics::MkStreamLabel,
    Self: svc::ExtractParam<metrics::labels::Route, http::Request<http::BoxBody>>,
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
                .check_new::<Self>()
                .check_new_service::<Self, http::Request<http::BoxBody>>()
                // The router does not take the backend's availability into
                // consideration, so we must eagerly fail requests to prevent
                // leaking tasks onto the runtime.
                .push_on_service(svc::LoadShed::layer())
                .push(filters::NewApplyFilters::<Self, _, _>::layer())
                .push(retry::NewHttpRetry::<Self, _>::layer(metrics.retry.clone()))
                .check_new::<Self>()
                .check_new_service::<Self, http::Request<http::BoxBody>>()
                // Set request extensions based on the route configuration
                // AND/OR headers
                .push(extensions::NewSetExtensions::layer())
                .push(metrics::layer(&metrics.requests, &metrics.body_data))
                .check_new::<Self>()
                .check_new_service::<Self, http::Request<http::BoxBody>>()
                // Configure a classifier to use in the endpoint stack.
                // TODO(ver) move this into NewSetExtensions?
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

impl<B, T> svc::ExtractParam<metrics::labels::Route, http::Request<B>> for Http<T> {
    fn extract_param(&self, req: &http::Request<B>) -> metrics::labels::Route {
        metrics::labels::Route::new(
            self.params.parent_ref.clone(),
            self.params.route_ref.clone(),
            self.params.params.export_hostname_labels.then(|| req.uri()),
        )
    }
}

impl<T> metrics::MkStreamLabel for Http<T> {
    type StatusLabels = metrics::labels::HttpRouteRsp;
    type DurationLabels = metrics::labels::Route;
    type StreamLabel = metrics::LabelHttpRouteRsp;

    fn mk_stream_labeler<B>(&self, req: &::http::Request<B>) -> Option<Self::StreamLabel> {
        let parent = self.params.parent_ref.clone();
        let route = self.params.route_ref.clone();
        let uri = self.params.params.export_hostname_labels.then(|| req.uri());
        Some(metrics::LabelHttpRsp::from(metrics::labels::Route::new(
            parent, route, uri,
        )))
    }
}

impl<T> svc::Param<extensions::Params> for Http<T> {
    fn param(&self) -> extensions::Params {
        let retry = self.params.params.retry.clone();
        extensions::Params {
            timeouts: self.params.params.timeouts.clone(),
            retry: retry.map(|r| retry::RetryPolicy {
                max_retries: r.max_retries as _,
                max_request_bytes: r.max_request_bytes,
                timeout: r.timeout,
                backoff: r.backoff,
                retryable_http_statuses: Some(r.status_ranges),
                retryable_grpc_statuses: None,
            }),
            allow_l5d_request_headers: self.params.params.allow_l5d_request_headers,
        }
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

impl<B, T> svc::ExtractParam<metrics::labels::Route, http::Request<B>> for Grpc<T> {
    fn extract_param(&self, req: &http::Request<B>) -> metrics::labels::Route {
        metrics::labels::Route::new(
            self.params.parent_ref.clone(),
            self.params.route_ref.clone(),
            self.params.params.export_hostname_labels.then(|| req.uri()),
        )
    }
}

impl<T> metrics::MkStreamLabel for Grpc<T> {
    type StatusLabels = metrics::labels::GrpcRouteRsp;
    type DurationLabels = metrics::labels::Route;
    type StreamLabel = metrics::LabelGrpcRouteRsp;

    fn mk_stream_labeler<B>(&self, req: &::http::Request<B>) -> Option<Self::StreamLabel> {
        let parent = self.params.parent_ref.clone();
        let route = self.params.route_ref.clone();
        let uri = self.params.params.export_hostname_labels.then(|| req.uri());
        Some(metrics::LabelGrpcRsp::from(metrics::labels::Route::new(
            parent, route, uri,
        )))
    }
}

impl<T> svc::Param<extensions::Params> for Grpc<T> {
    fn param(&self) -> extensions::Params {
        let retry = self.params.params.retry.clone();
        extensions::Params {
            timeouts: self.params.params.timeouts.clone(),
            retry: retry.map(|r| retry::RetryPolicy {
                max_retries: r.max_retries as _,
                max_request_bytes: r.max_request_bytes,
                timeout: r.timeout,
                backoff: r.backoff,
                retryable_http_statuses: None,
                retryable_grpc_statuses: Some(r.codes),
            }),
            allow_l5d_request_headers: self.params.params.allow_l5d_request_headers,
        }
    }
}

impl<T> svc::Param<classify::Request> for Grpc<T> {
    fn param(&self) -> classify::Request {
        classify::Request::ClientPolicy(
            classify::ClientPolicy::Grpc(policy::grpc::Codes::default()),
        )
    }
}
