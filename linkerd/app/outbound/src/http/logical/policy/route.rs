use super::super::Concrete;
use crate::{http::retry, RouteRef};
use linkerd_app_core::{classify, proxy::http, svc, Addr, Error, Result};
use linkerd_distribute as distribute;
use linkerd_http_route as http_route;
use linkerd_proxy_client_policy as policy;
use std::{fmt::Debug, hash::Hash, num::NonZeroU32, sync::Arc};

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
    pub(super) request_timeout: Option<std::time::Duration>,
    pub(super) retry_policy: Option<RouteRetryPolicy<E>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct RouteRetryPolicy<E> {
    pub(super) budget: policy::retry::Budget,
    pub(super) max_per_request: Option<NonZeroU32>,
    pub(super) retryable: E,
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
                // Sets an optional request timeout.
                .push(http::NewTimeout::layer())
                .push(classify::NewClassify::layer())
                .push(svc::NewMapErr::layer_with(|rt: &Self| {
                    let route = rt.params.route_ref.clone();
                    move |source| {
                        Error::from(RouteError {
                            route: route.clone(),
                            source,
                        })
                    }
                }))
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

impl<T, M, F, E> svc::Param<http::timeout::ResponseTimeout> for MatchedRoute<T, M, F, E> {
    fn param(&self) -> http::timeout::ResponseTimeout {
        http::timeout::ResponseTimeout(self.params.request_timeout)
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

impl<T> svc::Param<classify::Request> for Http<T> {
    fn param(&self) -> classify::Request {
        classify::Request::ClientPolicy(classify::ClientPolicy::Http(
            self.params.failure_policy.clone(),
        ))
    }
}

impl<T> svc::Param<Option<retry::Params>> for Http<T> {
    fn param(&self) -> Option<retry::Params> {
        let &RouteRetryPolicy {
            ref budget,
            max_per_request,
            ref retryable,
        } = self.params.retry_policy.as_ref()?;
        Some(retry::Params {
            budget: budget.clone().into(),
            max_per_request,
            profile_labels: None,
            response_classes: classify::Request::ClientPolicy(classify::ClientPolicy::Http(
                retryable.clone(),
            )),
        })
    }
}

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

impl<T> svc::Param<classify::Request> for Grpc<T> {
    fn param(&self) -> classify::Request {
        classify::Request::ClientPolicy(classify::ClientPolicy::Grpc(
            self.params.failure_policy.clone(),
        ))
    }
}

impl<T> svc::Param<Option<retry::Params>> for Grpc<T> {
    fn param(&self) -> Option<retry::Params> {
        // gRPC client policies cannot currently configure retries.
        // TODO: in the future, this should be implemented.
        None
    }
}

// === impl RouteRetryPolicy ===

impl<E> RouteRetryPolicy<E> {
    pub(super) fn new(
        budget: Option<policy::retry::Budget>,
        policy: Option<policy::retry::RoutePolicy<E>>,
    ) -> Option<Self> {
        let budget = budget?;
        let policy::retry::RoutePolicy {
            retryable,
            max_per_request,
        } = policy?;
        Some(Self {
            budget,
            max_per_request,
            retryable,
        })
    }
}
