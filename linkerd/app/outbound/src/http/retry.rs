use super::Route;
use futures::future;
use linkerd_app_core::{
    classify,
    http_metrics::retries::Handle,
    metrics, profiles,
    proxy::http::{ClientHandle, EraseResponse, HttpBody},
    svc::{layer, Either, Param},
    Error,
};
use linkerd_http_classify::{Classify, ClassifyEos, ClassifyResponse};
use linkerd_http_retry::{with_trailers, ReplayBody};
use linkerd_retry as retry;
use std::{future::Future, pin::Pin, sync::Arc};

pub fn layer<N, R>(
    metrics: metrics::HttpRouteRetry,
) -> impl layer::Layer<N, Service = retry::NewRetry<NewRetryPolicy, N, EraseResponse<()>, R>> + Clone
{
    retry::layer(NewRetryPolicy::new(metrics))
        // Because we wrap the response body type on retries, we must include a
        // `Proxy` middleware for unifying the response body types of the retry
        // and non-retry services.
        .proxy_on_response(EraseResponse::new(()))
}

#[derive(Clone, Debug)]
pub struct NewRetryPolicy {
    metrics: metrics::HttpRouteRetry,
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
    pub fn new(metrics: metrics::HttpRouteRetry) -> Self {
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

impl<A, B, E> retry::Policy<http::Request<ReplayBody<A>>, http::Response<with_trailers::Body<B>>, E>
    for RetryPolicy
where
    A: HttpBody + Unpin,
    A::Error: Into<Error>,
    B: HttpBody + Unpin,
{
    type Future = future::Ready<Self>;

    fn retry(
        &self,
        req: &http::Request<ReplayBody<A>>,
        result: Result<&http::Response<with_trailers::Body<B>>, &E>,
    ) -> Option<Self::Future> {
        let retryable = match result {
            Err(_) => false,
            Ok(rsp) => {
                // is the request a failure?
                let is_failure = classify::Request::from(self.response_classes.clone())
                    .classify(req)
                    .start(rsp)
                    .eos(rsp.body().trailers())
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
        // Since the body is already wrapped in a ReplayBody, it must not be obviously too large to
        // buffer/clone.
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

impl<A, B, E> retry::PrepareRetry<http::Request<A>, http::Response<B>, E> for RetryPolicy
where
    A: HttpBody + Unpin,
    A::Error: Into<Error>,
    B: HttpBody + Unpin + Send + 'static,
    B::Data: Unpin + Send,
    E: From<B::Error> + 'static,
{
    type RetryRequest = http::Request<ReplayBody<A>>;
    type RetryResponse = http::Response<with_trailers::Body<B>>;
    type ResponseFuture = Pin<Box<dyn Future<Output = Result<Self::RetryResponse, E>> + Send>>;

    fn prepare_request(
        &self,
        req: http::Request<A>,
    ) -> Either<Self::RetryRequest, http::Request<A>> {
        let (head, body) = req.into_parts();
        let replay_body = match ReplayBody::try_new(body, MAX_BUFFERED_BYTES) {
            Ok(body) => body,
            Err(body) => {
                tracing::debug!(
                    size = body.size_hint().lower(),
                    "Body is too large to buffer"
                );
                return Either::B(http::Request::from_parts(head, body));
            }
        };

        // The body may still be too large to be buffered if the body's length was not known.
        // `ReplayBody` handles this gracefully.
        Either::A(http::Request::from_parts(head, replay_body))
    }

    /// If the response is HTTP/2, return a future that checks for a `TRAILERS`
    /// frame immediately after the first frame of the response.
    fn prepare_response(rsp: http::Response<B>) -> Self::ResponseFuture {
        Box::pin(with_trailers::Body::map_response(rsp))
    }
}
