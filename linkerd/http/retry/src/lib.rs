#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

pub mod replay;
pub mod with_trailers;

pub use self::{replay::ReplayBody, with_trailers::TrailersBody};
pub use tower::retry::{budget::Budget, Policy};

use crate::with_trailers::ResponseWithTrailers;
use futures::future;
use linkerd_error::Error;
use linkerd_http_box::{BoxBody, BoxRequest, BoxResponse};
use linkerd_stack::{layer, ExtractParam, NewService, Service};
use std::task::{Context, Poll};
use tracing::{debug, trace};

#[derive(Clone, Debug)]
pub struct Params {
    pub max_request_bytes: usize,
}

/// Applies per-target retry policies.
#[derive(Clone, Debug)]
pub struct NewHttpRetry<P, X, N> {
    inner: N,
    extract: X,
    _marker: std::marker::PhantomData<fn() -> P>,
}

#[derive(Clone, Debug)]
pub struct HttpRetry<P, S> {
    inner: S,
    params: Params,
    _marker: std::marker::PhantomData<fn() -> P>,
}

// === impl NewRetry ===

impl<P, N> NewHttpRetry<P, (), N> {
    pub fn layer() -> impl layer::Layer<N, Service = Self> + Clone {
        Self::layer_via(())
    }
}

impl<P, X: Clone, N> NewHttpRetry<P, X, N> {
    pub fn layer_via(extract: X) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self {
            inner,
            extract: extract.clone(),
            _marker: std::marker::PhantomData,
        })
    }
}

impl<T, P, X, N> NewService<T> for NewHttpRetry<P, X, N>
where
    X: ExtractParam<Params, T>,
    N: NewService<T>,
{
    type Service = HttpRetry<P, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let params = self.extract.extract_param(&target);
        let inner = self.inner.new_service(target);
        HttpRetry {
            inner,
            params,
            _marker: std::marker::PhantomData,
        }
    }
}

// === impl Retry ===

type RetrySvc<P, S> =
    BoxResponse<tower::retry::Retry<P, ResponseWithTrailers<BoxRequest<ReplayBody, S>>>>;

impl<P, S> Service<http::Request<BoxBody>> for HttpRetry<P, S>
where
    P: Policy<http::Request<ReplayBody>, http::Response<TrailersBody>, Error>,
    P: Clone + Send + Sync + std::fmt::Debug + 'static,
    S: Service<http::Request<BoxBody>, Response = http::Response<BoxBody>, Error = Error> + Clone,
{
    type Response = http::Response<BoxBody>;
    type Error = Error;
    type Future = future::Either<
        <S as Service<http::Request<BoxBody>>>::Future,
        <RetrySvc<P, S> as Service<http::Request<ReplayBody>>>::Future,
    >;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<BoxBody>) -> Self::Future {
        let policy = match req.extensions().get::<P>() {
            Some(p) => p.clone(),
            // If there is no policy, there is no need to retry.
            None => {
                trace!(retryable = false, "Request lacks a retry policy");
                return future::Either::Left(self.inner.call(req));
            }
        };

        // Since this request is retryable, we need to setup the request body to
        // be buffered/cloneable. If the request body is too large to be cloned,
        // the retry policy is ignored.
        let req = {
            let (head, body) = req.into_parts();
            match ReplayBody::try_new(body, self.params.max_request_bytes) {
                Ok(body) => http::Request::from_parts(head, body),
                Err(body) => {
                    debug!(retryable = false, "Request body is too large to be retried");
                    return future::Either::Left(
                        self.inner.call(http::Request::from_parts(head, body)),
                    );
                }
            }
        };
        trace!(retryable = true, ?policy);

        // Take the inner service, replacing it with a clone. This allows the
        // readiness from poll_ready to be preserved.
        //
        // Retry::poll_ready is just a pass-through to the inner service, so we
        // can rely on the fact that we've taken the ready inner service handle.
        let pending = self.inner.clone();
        let inner = std::mem::replace(&mut self.inner, pending);
        let mut svc = BoxResponse::new(tower::retry::Retry::new(
            policy,
            ResponseWithTrailers(BoxRequest::new(inner)),
        ));

        future::Either::Right(svc.call(req))
    }
}
