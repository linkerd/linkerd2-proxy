use futures::{future, ready, Future, TryFuture, TryFutureExt};
use linkerd_app_core::{
    svc::{self, ExtractParam},
    Error, Result,
};
use linkerd_proxy_client_policy::{grpc, http};
use pin_project::pin_project;
use std::{
    marker::PhantomData,
    pin::Pin,
    task::{self, Context, Poll},
};

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

#[derive(Clone, Debug)]
#[pin_project]
pub struct ResponseFuture<A, F> {
    apply: A,

    #[pin]
    inner: F,
}

pub mod errors {
    use super::*;
    use std::sync::Arc;

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
    #[error("invalid client policy: {0}")]
    pub struct HttpInvalidPolicy(pub &'static str);
}

pub(crate) trait Apply {
    fn apply_request<B>(&self, req: &mut ::http::Request<B>) -> Result<()>;
    fn apply_response<B>(&self, rsp: &mut ::http::Response<B>) -> Result<()>;
}

pub fn apply_http_request<B>(
    r#match: &http::RouteMatch,
    filters: &[http::Filter],
    req: &mut ::http::Request<B>,
) -> Result<()> {
    // TODO Do any metrics apply here?
    for filter in filters {
        match filter {
            http::Filter::InjectFailure(fail) => {
                if let Some(http::filter::FailureResponse { status, message }) = fail.apply() {
                    return Err(errors::HttpRouteInjectedFailure { status, message }.into());
                }
            }

            http::Filter::Redirect(redir) => match redir.apply(req.uri(), r#match) {
                Ok(Some(http::filter::Redirection { status, location })) => {
                    return Err(errors::HttpRouteRedirect { status, location }.into());
                }

                Err(invalid) => {
                    return Err(errors::HttpRouteInvalidRedirect(invalid).into());
                }

                Ok(None) => {
                    tracing::debug!("Ignoring irrelevant redirect");
                }
            },

            http::Filter::RequestHeaders(rh) => {
                rh.apply(req.headers_mut());
            }

            http::Filter::InternalError(msg) => {
                return Err(errors::HttpInvalidPolicy(msg).into());
            }
            http::Filter::ResponseHeaders(_) => {} // ResponseHeaders filter does not apply to requests.
        }
    }

    Ok(())
}

pub fn apply_http_response<B>(
    filters: &[http::Filter],
    rsp: &mut ::http::Response<B>,
) -> Result<()> {
    // TODO Do any metrics apply here?
    for filter in filters {
        match filter {
            http::Filter::InjectFailure(_) => {} // InjectFailure filter does not apply to responses.
            http::Filter::Redirect(_) => {}      // Redirect filter does not apply to responses.
            http::Filter::RequestHeaders(_) => {} // RequestHeaders filter does not apply to responses.
            http::Filter::InternalError(_) => {} // InternalError filter does not apply to responses.
            http::Filter::ResponseHeaders(rh) => rh.apply(rsp.headers_mut()),
        }
    }

    Ok(())
}

pub fn apply_grpc_request<B>(
    _match: &grpc::RouteMatch,
    filters: &[grpc::Filter],
    req: &mut ::http::Request<B>,
) -> Result<()> {
    for filter in filters {
        match filter {
            grpc::Filter::InjectFailure(fail) => {
                if let Some(grpc::filter::FailureResponse { code, message }) = fail.apply() {
                    return Err(errors::GrpcRouteInjectedFailure { code, message }.into());
                }
            }

            grpc::Filter::RequestHeaders(rh) => {
                rh.apply(req.headers_mut());
            }

            grpc::Filter::InternalError(msg) => {
                return Err(errors::HttpInvalidPolicy(msg).into());
            }
        }
    }

    Ok(())
}

pub fn apply_grpc_response<B>(
    filters: &[grpc::Filter],
    _rsp: &mut ::http::Response<B>,
) -> Result<()> {
    for filter in filters {
        match filter {
            grpc::Filter::InjectFailure(_) => {} // InjectFailure filter does not apply to responses.
            grpc::Filter::RequestHeaders(_) => {} // RequestHeaders filter does not apply to responses.
            grpc::Filter::InternalError(_) => {} // InternalError filter does not apply to responses.
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
    A: Apply + Clone,
    S: svc::Service<::http::Request<B>, Response = ::http::Response<B>>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::Either<
        future::Ready<Result<Self::Response>>,
        future::ErrInto<ResponseFuture<A, S::Future>, Error>,
    >;

    #[inline]
    fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> task::Poll<Result<()>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, mut req: ::http::Request<B>) -> Self::Future {
        if let Err(e) = self.apply.apply_request(&mut req) {
            return future::Either::Left(future::err(e));
        }
        let rsp = ResponseFuture {
            apply: self.apply.clone(),
            inner: self.inner.call(req),
        };
        future::Either::Right(rsp.err_into::<Error>())
    }
}

// === impl ResponseFuture ===

impl<B, A, F> Future for ResponseFuture<A, F>
where
    A: Apply,
    F: TryFuture<Ok = ::http::Response<B>>,
    F::Error: Into<Error>,
{
    type Output = Result<::http::Response<B>>;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let mut out = ready!(this.inner.try_poll(cx));
        if let Ok(rsp) = &mut out {
            if let Err(e) = this.apply.apply_response(rsp) {
                return Poll::Ready(Err(e));
            };
        }
        Poll::Ready(out.map_err(Into::into))
    }
}
