use futures::{future, TryFutureExt};
use linkerd_app_core::{
    svc::{self, ExtractParam},
    Error, Result,
};
use linkerd_proxy_client_policy::{self as policy, grpc, http};
use std::{marker::PhantomData, sync::Arc, task};

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
pub struct NewApplyFilters<A, X, N> {
    inner: N,
    extract: X,
    _marker: PhantomData<fn() -> A>,
}

#[derive(Clone, Debug)]
pub struct ApplyFilters<A, S> {
    apply: A,
    inner: S,
}

#[derive(Debug, thiserror::Error)]
#[error("invalid redirect: {0}")]
pub struct HttpRouteInvalidRedirect(#[from] pub policy::http::filter::InvalidRedirect);

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

pub(crate) trait Apply {
    fn apply<B>(&self, req: &mut ::http::Request<B>) -> Result<()>;
}

pub fn apply_http<B>(
    r#match: &http::RouteMatch,
    filters: &[http::Filter],
    req: &mut ::http::Request<B>,
) -> Result<()> {
    // TODO Do any metrics apply here?
    for filter in filters {
        match filter {
            policy::http::Filter::InjectFailure(fail) => {
                if let Some(policy::http::filter::FailureResponse { status, message }) =
                    fail.apply()
                {
                    return Err(HttpRouteInjectedFailure { status, message }.into());
                }
            }

            policy::http::Filter::Redirect(redir) => match redir.apply(req.uri(), r#match) {
                Ok(Some(policy::http::filter::Redirection { status, location })) => {
                    return Err(HttpRouteRedirect { status, location }.into());
                }

                Err(invalid) => {
                    return Err(HttpRouteInvalidRedirect(invalid).into());
                }

                Ok(None) => {
                    tracing::debug!("Ignoring irrelevant redirect");
                }
            },

            policy::http::Filter::RequestHeaders(rh) => {
                rh.apply(req.headers_mut());
            }

            policy::http::Filter::InternalError(msg) => {
                return Err(HttpInvalidPolicy(msg).into());
            }
        }
    }

    Ok(())
}

pub fn apply_grpc<B>(
    _match: &grpc::RouteMatch,
    filters: &[grpc::Filter],
    req: &mut ::http::Request<B>,
) -> Result<()> {
    for filter in filters {
        match filter {
            policy::grpc::Filter::InjectFailure(fail) => {
                if let Some(policy::grpc::filter::FailureResponse { code, message }) = fail.apply()
                {
                    return Err(GrpcRouteInjectedFailure { code, message }.into());
                }
            }

            policy::grpc::Filter::RequestHeaders(rh) => {
                rh.apply(req.headers_mut());
            }

            policy::grpc::Filter::InternalError(msg) => {
                return Err(HttpInvalidPolicy(msg).into());
            }
        }
    }

    Ok(())
}

// === impl NewApplyFilters ===

impl<A, X: Clone, N> NewApplyFilters<A, X, N> {
    pub fn layer_via(extract: X) -> impl svc::layer::Layer<N, Service = Self> + Clone {
        svc::layer::mk(move |inner| Self {
            inner,
            extract: extract.clone(),
            _marker: PhantomData,
        })
    }
}

impl<A, N> NewApplyFilters<A, (), N> {
    pub fn layer() -> impl svc::layer::Layer<N, Service = Self> + Clone {
        Self::layer_via(())
    }
}

impl<T, A, X, N> svc::NewService<T> for NewApplyFilters<A, X, N>
where
    X: ExtractParam<A, T>,
    N: svc::NewService<T>,
{
    type Service = ApplyFilters<A, N::Service>;

    fn new_service(&self, params: T) -> Self::Service {
        let apply = self.extract.extract_param(&params);
        let inner = self.inner.new_service(params);
        ApplyFilters { apply, inner }
    }
}

// === impl ApplyFilters ===

impl<B, A, S> svc::Service<::http::Request<B>> for ApplyFilters<A, S>
where
    A: Apply,
    S: svc::Service<::http::Request<B>>,
    S::Error: Into<Error>,
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
        if let Err(e) = self.apply.apply(&mut req) {
            return future::Either::Left(future::err(e));
        }

        future::Either::Right(self.inner.call(req).err_into::<Error>())
    }
}
