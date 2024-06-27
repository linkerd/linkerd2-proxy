#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

pub mod replay;
pub mod with_trailers;

pub use self::{replay::ReplayBody, with_trailers::TrailersBody};
pub use tower::retry::budget::Budget;

use futures::future;
use linkerd_error::{Error, Result};
use linkerd_http_box::BoxBody;
use linkerd_stack::{layer, Param, Service};
use std::{
    future::Future,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};
use tower::ServiceExt;
use tracing::{debug, trace};

/// A HTTP retry strategy.
pub trait Policy: Clone + Sized {
    type Future: Future<Output = ()> + Send + 'static;

    /// Returns true when no further retries
    fn exhausted(&self) -> bool;

    /// Prepare headers for the next request.
    fn set_headers(&self, dst: &mut http::HeaderMap, orig: &http::HeaderMap) {
        *dst = orig.clone();
    }

    /// Prepare extensions for the next request.
    fn set_extensions(&self, _dst: &mut http::Extensions, _orig: &http::Extensions) {}

    /// Determines if a response should be retried.
    fn retry(
        &mut self,
        result: Result<&http::Response<TrailersBody>, &Error>,
    ) -> Option<Self::Future>;
}

#[derive(Clone, Debug)]
pub struct Params {
    pub max_request_bytes: usize,
}

/// A Retry middleware that attempts to extract a `P` typed request extension to
/// instrument retries. When the request extension is not set, requests are not
/// retried.
#[derive(Clone, Debug)]
pub struct HttpRetry<P, S> {
    inner: S,
    _marker: PhantomData<fn() -> P>,
}

// === impl HttpRetry ===

impl<P, N> HttpRetry<P, N> {
    pub fn layer() -> impl layer::Layer<N, Service = Self> + Copy {
        layer::mk(|inner| Self {
            inner,
            _marker: PhantomData,
        })
    }
}

impl<P, S> Service<http::Request<BoxBody>> for HttpRetry<P, S>
where
    P: Policy,
    P: Param<Params>,
    P: Clone + Send + Sync + std::fmt::Debug + 'static,
    S: Service<http::Request<BoxBody>, Response = http::Response<BoxBody>, Error = Error>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = http::Response<BoxBody>;
    type Error = Error;
    type Future = future::Either<
        <S as Service<http::Request<BoxBody>>>::Future,
        Pin<Box<dyn Future<Output = Result<http::Response<BoxBody>>> + Send + 'static>>,
    >;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<BoxBody>) -> Self::Future {
        // Retries are configured from request extensions so that they can be
        // configured from both policy and request headers.
        let Some(policy) = req.extensions().get::<P>().cloned() else {
            // If there is no policy, there is no need to retry. This avoids
            // buffering logic in the default case.
            trace!(retryable = false, "Request lacks a retry policy");
            return future::Either::Left(self.inner.call(req));
        };

        // Since this request is retryable, we need to setup the request body to
        // be buffered/cloneable. If the request body is too large to be cloned,
        // the retry policy is ignored.
        let req = {
            let (head, body) = req.into_parts();
            let Params { max_request_bytes } = policy.param();
            match ReplayBody::try_new(body, max_request_bytes) {
                Ok(body) => http::Request::from_parts(head, body),
                Err(body) => {
                    debug!(retryable = false, "Request body is too large to be retried");
                    return future::Either::Left(
                        self.inner.call(http::Request::from_parts(head, body)),
                    );
                }
            }
        };
        debug!(retryable = true, policy = ?policy);

        // Take the inner service, replacing it with a clone. This allows the
        // readiness from poll_ready to be preserved.
        //
        // Retry::poll_ready is just a pass-through to the inner service, so we
        // can rely on the fact that we've taken the ready inner service handle.
        let pending = self.inner.clone();
        let inner = std::mem::replace(&mut self.inner, pending);
        future::Either::Right(Box::pin(dispatch(inner, policy, req)))
    }
}

async fn dispatch(
    // `svc` must be made ready before calling this function.
    mut svc: impl Service<http::Request<BoxBody>, Response = http::Response<BoxBody>, Error = Error>,
    mut policy: impl Policy,
    mut request: http::Request<ReplayBody>,
) -> Result<http::Response<BoxBody>> {
    loop {
        if policy.exhausted() || request.body().is_capped() {
            return svc.call(request.map(BoxBody::new)).await;
        }

        let backup = mk_backup(&request, &policy);

        // Make the request and wait for the response. We proactively poll the
        // response body for
        let res = match svc.call(request.map(BoxBody::new)).await {
            Ok(rsp) => {
                let rsp = TrailersBody::map_response(rsp).await;
                Ok(rsp)
            }
            Err(e) => Err(e),
        };

        let Some(backoff) = policy.retry(res.as_ref()) else {
            return res.map(|rsp| rsp.map(BoxBody::new));
        };

        let (_, ready) = tokio::join!(backoff, svc.ready());
        ready?;

        request = backup;
    }
}

fn mk_backup(orig: &http::Request<ReplayBody>, policy: &impl Policy) -> http::Request<ReplayBody> {
    let mut dst = http::Request::new(orig.body().clone());
    *dst.method_mut() = orig.method().clone();
    *dst.uri_mut() = orig.uri().clone();
    *dst.version_mut() = orig.version();
    policy.set_headers(dst.headers_mut(), orig.headers());
    policy.set_extensions(dst.extensions_mut(), orig.extensions());
    dst
}
