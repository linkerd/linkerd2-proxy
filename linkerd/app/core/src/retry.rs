use super::classify;
use super::dst::Route;
use super::http_metrics::retries::Handle;
use super::metrics::HttpRouteRetry;
use crate::profiles;
use futures::future;
use hyper::body::HttpBody;
use linkerd_http_classify::{Classify, ClassifyEos, ClassifyResponse};
use linkerd_retry::NewRetryLayer;
use linkerd_stack::Param;
use std::marker::PhantomData;
use std::sync::Arc;
use tower::retry::budget::Budget;

pub use linkerd_retry::*;

pub fn layer(metrics: HttpRouteRetry) -> NewRetryLayer<NewRetry<WithBody>> {
    NewRetryLayer::new(NewRetry::new(metrics).clone_requests_via::<WithBody>())
}

pub trait CloneRequest<Req> {
    fn clone_request(req: &Req) -> Option<Req>;
}

#[derive(Clone, Debug)]
pub struct NewRetry<C = ()> {
    metrics: HttpRouteRetry,
    _clone_request: PhantomData<C>,
}

pub struct Retry<C = ()> {
    metrics: Handle,
    budget: Arc<Budget>,
    response_classes: profiles::http::ResponseClasses,
    _clone_request: PhantomData<C>,
}

#[derive(Clone, Debug)]
pub struct WithBody;

impl NewRetry {
    pub fn new(metrics: HttpRouteRetry) -> Self {
        Self {
            metrics,
            _clone_request: PhantomData,
        }
    }

    pub fn clone_requests_via<C>(self) -> NewRetry<C> {
        NewRetry {
            metrics: self.metrics,
            _clone_request: PhantomData,
        }
    }
}

impl<C> linkerd_retry::NewPolicy<Route> for NewRetry<C> {
    type Policy = Retry<C>;

    fn new_policy(&self, route: &Route) -> Option<Self::Policy> {
        let retries = route.route.retries().cloned()?;

        let metrics = self.metrics.get_handle(route.param());
        Some(Retry {
            metrics,
            budget: retries.budget().clone(),
            response_classes: route.route.response_classes().clone(),
            _clone_request: self._clone_request,
        })
    }
}

impl<C, A, B, E> linkerd_retry::Policy<http::Request<A>, http::Response<B>, E> for Retry<C>
where
    C: CloneRequest<http::Request<A>>,
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
        C::clone_request(req)
    }
}

impl<C> Clone for Retry<C> {
    fn clone(&self) -> Self {
        Self {
            metrics: self.metrics.clone(),
            budget: self.budget.clone(),
            response_classes: self.response_classes.clone(),
            _clone_request: self._clone_request,
        }
    }
}

impl<B: Default + HttpBody> CloneRequest<http::Request<B>> for () {
    fn clone_request(req: &http::Request<B>) -> Option<http::Request<B>> {
        if !req.body().is_end_stream() {
            return None;
        }

        let mut clone = http::Request::new(B::default());
        *clone.method_mut() = req.method().clone();
        *clone.uri_mut() = req.uri().clone();
        *clone.headers_mut() = req.headers().clone();
        *clone.version_mut() = req.version();

        Some(clone)
    }
}

// === impl WithBody ===

impl WithBody {
    /// Allow buffering requests up to 64 kb
    pub const MAX_BUFFERED_BYTES: usize = 64 * 1024;
}

impl<B: HttpBody + Unpin + Default> CloneRequest<http::Request<replay::ReplayBody<B>>>
    for WithBody
{
    fn clone_request(
        req: &http::Request<replay::ReplayBody<B>>,
    ) -> Option<http::Request<replay::ReplayBody<B>>> {
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
        if has_body && content_length(&req).unwrap_or(usize::MAX) > Self::MAX_BUFFERED_BYTES {
            tracing::trace!(
                req.has_body = has_body,
                req.content_length = ?content_length(&req),
                "not retryable",
            );
            return None;
        }

        tracing::trace!(
            req.has_body = has_body,
            req.content_length = ?content_length(&req),
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
