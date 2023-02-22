use super::MatchedRouteParams;
use futures::{future, TryFutureExt};
use linkerd_app_core::{svc, Error, Result};
use linkerd_http_route as route;
use linkerd_proxy_client_policy::{grpc, http};
use std::{sync::Arc, task};

// #[cfg(test)]
// mod tests;

/// A middleware that enforces policy on each HTTP request.
///
/// This enforcement is done lazily on each request so that policy updates are
/// honored as the connection progresses.
///
/// The inner service is created for each request, so it's expected that this is
/// combined with caching.
#[derive(Clone, Debug)]
pub struct NewApplyFilters<N> {
    inner: N,
}

#[derive(Clone, Debug)]
pub struct ApplyFilters<M, F, S> {
    inner: S,
    filters: Filters<M, F>,
}

#[derive(Clone, Debug)]
pub(crate) struct Filters<M, F> {
    r#match: route::RouteMatch<M>,
    filters: Arc<[F]>,
}

pub(crate) trait Apply {
    fn apply<B>(&self, req: &mut ::http::Request<B>) -> Result<()>;
}

#[derive(Debug, thiserror::Error)]
#[error("invalid redirect: {0}")]
pub struct HttpRouteInvalidRedirect(#[from] pub http::filter::InvalidRedirect);

#[derive(Debug, thiserror::Error)]
#[error("request redirected to {location}")]
pub struct HttpRouteRedirect {
    pub status: ::http::StatusCode,
    pub location: ::http::Uri,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
#[error("HTTP request configured to fail with {status}: {message}")]
pub struct HttpRouteInjectedFailure {
    pub status: ::http::StatusCode,
    pub message: Arc<str>,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
#[error("gRPC request configured to fail with {code}: {message}")]
pub struct GrpcRouteInjectedFailure {
    pub code: u16,
    pub message: Arc<str>,
}

#[derive(Debug, thiserror::Error)]
#[error("invalid server policy: {0}")]
pub struct HttpInvalidPolicy(&'static str);

// === impl NewApplyFilters ===

impl<N> NewApplyFilters<N> {
    pub fn layer() -> impl svc::layer::Layer<N, Service = Self> + Clone + Copy {
        svc::layer::mk(|inner| Self { inner })
    }
}

impl<T, M, F, N> svc::NewService<MatchedRouteParams<T, M, F>> for NewApplyFilters<N>
where
    T: Clone,
    M: Clone,
    F: Clone,
    N: svc::NewService<MatchedRouteParams<T, M, F>>,
{
    type Service = ApplyFilters<M, F, N::Service>;

    fn new_service(&self, params: MatchedRouteParams<T, M, F>) -> Self::Service {
        let filters = Filters {
            r#match: params.r#match.clone(),
            filters: params.params.filters.clone(),
        };
        let inner = self.inner.new_service(params);
        ApplyFilters { filters, inner }
    }
}

// === impl ApplyFilters ===

impl<B, M, F, S> svc::Service<::http::Request<B>> for ApplyFilters<M, F, S>
where
    S: svc::Service<::http::Request<B>>,
    S::Error: Into<Error>,
    Filters<M, F>: Apply,
{
    type Response = S::Response;
    type Error = Error;
    type Future =
        future::Either<future::Ready<Result<Self::Response>>, future::ErrInto<S::Future, Error>>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> task::Poll<Result<()>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, mut req: ::http::Request<B>) -> Self::Future {
        if let Err(e) = self.filters.apply(&mut req) {
            return future::Either::Left(future::err(e));
        }

        future::Either::Right(self.inner.call(req).err_into::<Error>())
    }
}

impl Apply for Filters<route::http::r#match::RequestMatch, http::Filter> {
    fn apply<B>(&self, req: &mut ::http::Request<B>) -> Result<()> {
        // TODO Do any metrics apply here?
        for filter in &*self.filters {
            match filter {
                http::Filter::InjectFailure(fail) => {
                    if let Some(http::filter::FailureResponse { status, message }) = fail.apply() {
                        return Err(HttpRouteInjectedFailure { status, message }.into());
                    }
                }

                http::Filter::Redirect(redir) => match redir.apply(req.uri(), &self.r#match) {
                    Ok(Some(http::filter::Redirection { status, location })) => {
                        return Err(HttpRouteRedirect { status, location }.into());
                    }

                    Err(invalid) => {
                        return Err(HttpRouteInvalidRedirect(invalid).into());
                    }

                    Ok(None) => {
                        tracing::debug!("Ignoring irrelevant redirect");
                    }
                },

                http::Filter::RequestHeaders(rh) => {
                    rh.apply(req.headers_mut());
                }

                http::Filter::InternalError(msg) => {
                    return Err(HttpInvalidPolicy(msg).into());
                }
            }
        }

        Ok(())
    }
}

impl Apply for Filters<route::grpc::r#match::RouteMatch, grpc::Filter> {
    fn apply<B>(&self, req: &mut ::http::Request<B>) -> Result<()> {
        for filter in &*self.filters {
            match filter {
                grpc::Filter::InjectFailure(fail) => {
                    if let Some(grpc::filter::FailureResponse { code, message }) = fail.apply() {
                        return Err(GrpcRouteInjectedFailure { code, message }.into());
                    }
                }

                grpc::Filter::RequestHeaders(rh) => {
                    rh.apply(req.headers_mut());
                }

                grpc::Filter::InternalError(msg) => {
                    return Err(HttpInvalidPolicy(msg).into());
                }
            }
        }

        Ok(())
    }
}
