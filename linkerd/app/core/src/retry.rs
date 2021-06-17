use super::classify;
use super::dst::Route;
use super::http_metrics::retries::Handle;
use super::metrics::HttpRouteRetry;
use crate::profiles;
use futures::future;
use linkerd_error::Error;
use linkerd_http_classify::{Classify, ClassifyEos, ClassifyResponse};
use linkerd_http_retry::ReplayBody;
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

/// Allow buffering requests up to 64 kb
const MAX_BUFFERED_BYTES: usize = 64 * 1024;

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
    fn can_retry<A: http_body::Body>(&self, req: &http::Request<A>) -> bool {
        let content_length = |req: &http::Request<_>| {
            req.headers()
                .get(http::header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok()?.parse::<usize>().ok())
        };

        // Requests without bodies can always be retried, as we will not need to
        // buffer the body. If the request *does* have a body, retry it if and
        // only if the request contains a `content-length` header and the
        // content length is >= 64 kb.
        let has_body = !req.body().is_end_stream();
        if has_body && content_length(&req).unwrap_or(usize::MAX) > MAX_BUFFERED_BYTES {
            tracing::trace!(
                req.has_body = has_body,
                req.content_length = ?content_length(&req),
                "not retryable",
            );
            return false;
        }

        tracing::trace!(
            req.has_body = has_body,
            req.content_length = ?content_length(&req),
            "retryable",
        );
        true
    }
}

impl<A, B, E> retry::Policy<http::Request<A>, http::Response<B>, E> for RetryPolicy
where
    A: http_body::Body + Clone,
{
    type Future = future::Ready<Self>;

    fn retry(
        &self,
        req: &http::Request<A>,
        result: Result<&http::Response<B>, &E>,
    ) -> Option<Self::Future> {
        let retryable = match result {
            Err(_) => false,
            Ok(rsp) => classify::Request::from(self.response_classes.clone())
                .classify(req)
                .start(rsp)
                .eos(None)
                .is_failure(),
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

    fn clone_request(&self, req: &http::Request<A>) -> Option<http::Request<A>> {
        let can_retry = self.can_retry(&req);
        debug_assert!(
            can_retry,
            "The retry policy attempted to clone an un-retryable request. This is unexpected."
        );
        if !can_retry {
            return None;
        }

        let mut clone = http::Request::new(req.body().clone());
        *clone.method_mut() = req.method().clone();
        *clone.uri_mut() = req.uri().clone();
        *clone.headers_mut() = req.headers().clone();
        *clone.version_mut() = req.version();

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
        if self.can_retry(&req) {
            return Either::A(req.map(|body| ReplayBody::new(body, MAX_BUFFERED_BYTES)));
        }
        Either::B(req)
    }
}
