use super::classify;
use super::dst::Route;
use super::http_metrics::retries::Handle;
use super::metrics::HttpRouteRetry;
use crate::profiles;
use futures::future;
use http_body::SizeHint;
use linkerd_error::Error;
use linkerd_http_classify::{Classify, ClassifyEos, ClassifyResponse};
use linkerd_http_retry::ReplayBody;
use linkerd_proxy_http::ClientHandle;
use linkerd_retry as retry;
use linkerd_stack::{layer, Either, Param};
use std::sync::Arc;

pub fn layer<N>(
    metrics: HttpRouteRetry,
) -> impl layer::Layer<N, Service = retry::NewRetry<NewRetryPolicy, N>> + Clone {
    retry::NewRetry::<_, N>::layer(NewRetryPolicy::new(metrics))
}

#[derive(Clone, Debug)]
pub struct NewRetryPolicy {
    metrics: HttpRouteRetry,
}

#[derive(Clone, Debug)]
pub struct RetryPolicy {
    metrics: Handle,
    budget: Arc<retry::Budget>,
    response_classes: profiles::http::ResponseClasses,
}

// === impl NewRetryPolicy ===

impl NewRetryPolicy {
    pub fn new(metrics: HttpRouteRetry) -> Self {
        Self { metrics }
    }
}

impl retry::NewPolicy<Route> for NewRetryPolicy {
    type Policy = RetryPolicy;

    fn new_policy(&self, route: &Route) -> Option<Self::Policy> {
        let retries = route.route.retries().cloned()?;

        let metrics = self.metrics.get_handle(route.param());
        Some(RetryPolicy {
            metrics,
            budget: retries.budget().clone(),
            response_classes: route.route.response_classes().clone(),
        })
    }
}

// === impl Retry ===

impl RetryPolicy {
    /// Allow buffering requests up to 64 kb
    const MAX_BUFFERED_BYTES: usize = 64 * 1024;

    /// Checks whether the request is known to be too large to buffer (without actually reading its
    /// body).
    ///
    /// Returns false unless the request is known to be larger than `MAX_BUFFERED_BYTES`.
    fn body_too_big<A: http_body::Body>(req: &http::Request<A>) -> bool {
        // Use the lower bound of the size hint to determine whether we may be able to buffer the
        // entire body. If the body ends up larger than that, the `ReplayBody` should handle that
        // case gracefully.
        //
        // If a `content-length` was specified the size hint will be set to that value.
        let size = req.body().size_hint().lower();
        let too_big = size > Self::MAX_BUFFERED_BYTES as u64;
        tracing::trace!(body.size = ?size, %too_big);
        too_big
    }
}

impl<A, B, E> retry::Policy<http::Request<ReplayBody<A>>, http::Response<B>, E> for RetryPolicy
where
    A: http_body::Body + Unpin,
    A::Error: Into<Error>,
{
    type Future = future::Ready<Self>;

    fn retry(
        &self,
        req: &http::Request<ReplayBody<A>>,
        result: Result<&http::Response<B>, &E>,
    ) -> Option<Self::Future> {
        let retryable = match result {
            Err(_) => false,
            Ok(rsp) => {
                // is the request a failure?
                let is_failure = classify::Request::from(self.response_classes.clone())
                    .classify(req)
                    .start(rsp)
                    .eos(None)
                    .is_failure();
                // did the body exceed the maximum length limit?
                let exceeded_max_len = req.body().is_capped();
                let retryable = is_failure && !exceeded_max_len;
                tracing::trace!(is_failure, exceeded_max_len, retryable);
                retryable
            }
        };

        if !retryable {
            self.budget.deposit();
            return None;
        }

        let withdrew = self.budget.withdraw().is_ok();
        self.metrics.incr_retryable(withdrew);
        if !withdrew {
            return None;
        }

        Some(future::ready(self.clone()))
    }

    fn clone_request(
        &self,
        req: &http::Request<ReplayBody<A>>,
    ) -> Option<http::Request<ReplayBody<A>>> {
        let body_too_big = Self::body_too_big(req);
        debug_assert!(
            !body_too_big,
            "The retry policy attempted to clone an un-retryable request. This is unexpected."
        );
        if body_too_big {
            return None;
        }

        let mut clone = http::Request::new(req.body().clone());
        *clone.method_mut() = req.method().clone();
        *clone.uri_mut() = req.uri().clone();
        *clone.headers_mut() = req.headers().clone();
        *clone.version_mut() = req.version();

        // The HTTP server sets a ClientHandle with the client's address and a means to close the
        // server-side connection.
        if let Some(client_handle) = req.extensions().get::<ClientHandle>().cloned() {
            clone.extensions_mut().insert(client_handle);
        }

        Some(clone)
    }
}

impl<A, B, E> retry::PrepareRequest<http::Request<A>, http::Response<B>, E> for RetryPolicy
where
    A: http_body::Body + Unpin,
    A::Error: Into<Error>,
{
    type RetryRequest = http::Request<ReplayBody<A>>;

    fn prepare_request(
        &self,
        req: http::Request<A>,
    ) -> Either<Self::RetryRequest, http::Request<A>> {
        if Self::body_too_big(&req) {
            return Either::B(req);
        }

        // The body may still be too large to be buffered (if the body's length was not known).
        // `ReplayBody` handles this gracefully.
        Either::A(req.map(|body| ReplayBody::new(body, Self::MAX_BUFFERED_BYTES)))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn body_too_big() {
        let mk_body =
            |sz: usize| -> hyper::Body { (0..sz).map(|_| "x").collect::<String>().into() };
        let mk_req = |sz: usize| {
            http::Request::builder()
                .header(http::header::CONTENT_LENGTH, sz)
                .body(mk_body(sz))
                .unwrap()
        };

        assert!(
            !RetryPolicy::body_too_big(&mk_req(0)),
            "empty body is not too big"
        );

        assert!(
            !RetryPolicy::body_too_big(&mk_req(RetryPolicy::MAX_BUFFERED_BYTES)),
            "body at maximum capacity is not too big"
        );

        assert!(
            RetryPolicy::body_too_big(&mk_req(RetryPolicy::MAX_BUFFERED_BYTES + 1)),
            "over-sized body is considered too big"
        );

        let req = http::Request::builder()
            .body(mk_body(RetryPolicy::MAX_BUFFERED_BYTES * 2))
            .unwrap();
        assert!(!req.headers().contains_key(http::header::CONTENT_LENGTH));
        assert!(
            RetryPolicy::body_too_big(&req),
            "over-sized body without content-length is considered too big if the size hint is known"
        );

        let (_sender, body) = hyper::Body::channel();
        let req = http::Request::builder().body(body).unwrap();
        assert!(!req.headers().contains_key(http::header::CONTENT_LENGTH));
        assert!(
            !RetryPolicy::body_too_big(&req),
            "body without content-length is not considered too big if the size hint is not known"
        );
    }
}
