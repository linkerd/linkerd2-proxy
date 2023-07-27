use super::{super::Concrete, filters};
use crate::{BackendRef, RouteRef};
use linkerd_app_core::{errors, proxy::http, svc, Error, Result};
use linkerd_http_route as http_route;
use linkerd_proxy_client_policy as policy;
use std::{fmt::Debug, future::Future, hash::Hash, sync::Arc};

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

#[derive(Clone, Debug)]
pub struct ExtractMetrics {
    metrics: RouteBackendMetrics,
}

/// Wraps errors with backend metadata.
#[derive(Debug, thiserror::Error)]
#[error("backend {}: {source}", backend.0)]
struct BackendError {
    backend: BackendRef,
    #[source]
    source: Error,
}

/// Synthesizes 504 Gateway Timeout responses for the `timeouts.backend_request`
/// timeout.
///
/// This is necessary because we want these timeouts to be retried, and the
/// retry layer only retries requests that fail with an HTTP status code, rather
/// than for requests that fail with Rust `Err`s. Errors returned by the
/// `MatchedBackend` stack are converted to HTTP responses by the `ServerRescue`
/// layer, which is much higher up the stack than the `retry` layer, which is in
/// the `MatchedRoute` stack. Therefore, it's necessary to eagerly synthesize
/// responses for the `backend_request` timeout, so that the retry layer sees
/// them as 504 responses rather than as `Err`s.
#[derive(Copy, Clone, Debug)]
struct TimeoutRescue;

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
    T: svc::Param<errors::respond::EmitHeaders>,
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
                    Future = impl Future<Output = Result<http::Response<http::BoxBody>, Error>> + Send,
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
                .push(http::NewTimeout::layer())
                .push(count_reqs::NewCountRequests::layer_via(ExtractMetrics {
                    metrics: metrics.clone(),
                }))
                .push(svc::NewMapErr::layer_with(|t: &Self| {
                    let backend = t.params.concrete.backend_ref.clone();
                    move |source| {
                        Error::from(BackendError {
                            backend: backend.clone(),
                            source,
                        })
                    }
                }))
                // Eagerly synthesize 504 responses for backend_request timeout
                // errors.
                // See the doc comment on `TimeoutRescue` for details.
                .push(TimeoutRescue::layer())
                .push_on_service(http::BoxResponse::<_>::layer())
                .push(svc::ArcNewService::layer())
                .into_inner()
        })
    }
}

impl<T, M, F> svc::Param<http::ResponseTimeout> for MatchedBackend<T, M, F> {
    fn param(&self) -> http::ResponseTimeout {
        http::ResponseTimeout(self.params.request_timeout)
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

impl<T> svc::ExtractParam<RequestCount, Http<T>> for ExtractMetrics {
    fn extract_param(&self, params: &Http<T>) -> RequestCount {
        RequestCount(self.metrics.http_requests_total(
            params.params.concrete.parent_ref.clone(),
            params.params.route_ref.clone(),
            params.params.concrete.backend_ref.clone(),
        ))
    }
}

impl<T> svc::ExtractParam<RequestCount, Grpc<T>> for ExtractMetrics {
    fn extract_param(&self, params: &Grpc<T>) -> RequestCount {
        RequestCount(self.metrics.grpc_requests_total(
            params.params.concrete.parent_ref.clone(),
            params.params.route_ref.clone(),
            params.params.concrete.backend_ref.clone(),
        ))
    }
}

// === impl TimeoutRescue ===

impl TimeoutRescue {
    pub fn layer<N>(
    ) -> impl svc::layer::Layer<N, Service = errors::NewRespondService<Self, Self, N>> + Clone {
        errors::respond::layer(Self)
    }
}

impl<T> svc::ExtractParam<Self, T> for TimeoutRescue {
    #[inline]
    fn extract_param(&self, _: &T) -> Self {
        *self
    }
}

impl<T, M, F> svc::ExtractParam<errors::respond::EmitHeaders, MatchedBackend<T, M, F>>
    for TimeoutRescue
where
    Concrete<T>: svc::Param<errors::respond::EmitHeaders>,
{
    #[inline]
    fn extract_param(&self, target: &MatchedBackend<T, M, F>) -> errors::respond::EmitHeaders {
        svc::Param::param(&target.params.concrete)
    }
}

impl errors::HttpRescue<Error> for TimeoutRescue {
    fn rescue(&self, error: Error) -> Result<errors::SyntheticHttpResponse> {
        // A policy configured request timeout was encountered.
        if errors::is_caused_by::<http::ResponseTimeoutError>(&*error) {
            return Ok(errors::SyntheticHttpResponse::gateway_timeout(error));
        }

        // Forward any other error up-stack to be handled by a higher-level
        // `HttpRescue` layer.
        Err(error)
    }
}
