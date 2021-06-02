use super::classify;
use super::dst::Route;
use super::http_metrics::retries::Handle;
use super::metrics::HttpRouteRetry;
use crate::profiles;
use futures::future;
use http_body::Body;
use linkerd_http_classify::{Classify, ClassifyEos, ClassifyResponse};
use linkerd_stack::Param;
use std::sync::Arc;
use tower::retry::budget::Budget;

pub use linkerd_http_retry::*;

pub fn layer(new_retry: NewRetry) -> NewRetryLayer<NewRetry> {
    NewRetryLayer::new(new_retry)
}

pub trait CloneRequest<Req> {
    fn clone_request(&self, req: &Req) -> Option<Req>;
}

#[derive(Clone, Debug)]
pub struct NewRetry<C = BufferBody> {
    metrics: HttpRouteRetry,
    clone_request: C,
}

pub struct Retry<C = BufferBody> {
    metrics: Handle,
    budget: Arc<Budget>,
    response_classes: profiles::http::ResponseClasses,
    clone_request: C,
}

#[derive(Copy, Clone, Debug)]
pub struct BufferBody {
    max_length_bytes: usize,
}

impl NewRetry {
    pub fn new(metrics: HttpRouteRetry) -> Self {
        Self {
            metrics,
            clone_request: BufferBody::default(),
        }
    }

    pub fn clone_requests_via<C: Clone>(self, clone_request: C) -> NewRetry<C> {
        NewRetry {
            metrics: self.metrics,
            clone_request,
        }
    }
}

impl<C: Clone> NewPolicy<Route> for NewRetry<C> {
    type Policy = Retry<C>;

    fn new_policy(&self, route: &Route) -> Option<Self::Policy> {
        let retries = route.route.retries().cloned()?;

        let metrics = self.metrics.get_handle(route.param());
        Some(Retry {
            metrics,
            budget: retries.budget().clone(),
            response_classes: route.route.response_classes().clone(),
            clone_request: self.clone_request.clone(),
        })
    }
}

impl<C, A, B, E> Policy<http::Request<A>, http::Response<B>, E> for Retry<C>
where
    C: CloneRequest<http::Request<A>> + Clone,
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
        self.clone_request.clone_request(req)
    }
}

impl<C: Clone> Clone for Retry<C> {
    fn clone(&self) -> Self {
        Self {
            metrics: self.metrics.clone(),
            budget: self.budget.clone(),
            response_classes: self.response_classes.clone(),
            clone_request: self.clone_request.clone(),
        }
    }
}

// === impl BufferBody ===

impl BufferBody {
    /// Allow buffering requests up to 64 kb
    pub const DEFAULT_MAX_BUFFERED_BYTES: usize = 64 * 1024;

    pub fn with_max_length(max_length_bytes: usize) -> Self {
        Self { max_length_bytes }
    }
}

impl<B: Body + Unpin> CloneRequest<http::Request<ReplayBody<B>>> for BufferBody {
    fn clone_request(
        &self,
        req: &http::Request<ReplayBody<B>>,
    ) -> Option<http::Request<ReplayBody<B>>> {
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
        if has_body && content_length(&req).unwrap_or(usize::MAX) > self.max_length_bytes {
            tracing::trace!(
                req.has_body = has_body,
                req.content_length = ?content_length(&req),
                max_content_length = self.max_length_bytes,
                "not retryable",
            );
            return None;
        }

        tracing::trace!(
            req.has_body = has_body,
            req.content_length = ?content_length(&req),
            max_content_length = self.max_length_bytes,
            "retryable",
        );

        let mut clone = http::Request::new(req.body().clone());
        *clone.method_mut() = req.method().clone();
        *clone.uri_mut() = req.uri().clone();
        *clone.headers_mut() = req.headers().clone();
        *clone.version_mut() = req.version();

        Some(clone)
    }
}

impl Default for BufferBody {
    fn default() -> Self {
        Self {
            max_length_bytes: Self::DEFAULT_MAX_BUFFERED_BYTES,
        }
    }
}
