use futures::{future, FutureExt};
use linkerd_app_core::{
    classify,
    http_metrics::retries::Handle,
    metrics::{self, ProfileRouteLabels},
    proxy::http::{ClientHandle, EraseResponse, HttpBody},
    svc::{layer, Either, Param},
    Error,
};
use linkerd_http_classify::{Classify, ClassifyEos, ClassifyResponse};
use linkerd_http_retry::{
    with_trailers::{self, WithTrailers},
    ReplayBody,
};
use linkerd_retry as retry;
pub use retry::Budget;
use std::{num::NonZeroU32, sync::Arc};

pub fn layer<N>(
    metrics: metrics::HttpProfileRouteRetry,
) -> impl layer::Layer<N, Service = retry::NewRetry<NewRetryPolicy, N, EraseResponse<()>>> + Clone {
    retry::layer(NewRetryPolicy::new(metrics))
        // Because we wrap the response body type on retries, we must include a
        // `Proxy` middleware for unifying the response body types of the retry
        // and non-retry services.
        .with_proxy(EraseResponse::new(()))
}

#[derive(Clone, Debug)]
pub struct Params {
    pub budget: Arc<Budget>,
    pub max_per_request: Option<NonZeroU32>,
    pub profile_labels: Option<ProfileRouteLabels>,
    pub response_classes: classify::Request,
}

#[derive(Clone, Debug)]
pub struct NewRetryPolicy {
    metrics: metrics::HttpProfileRouteRetry,
}

#[derive(Clone, Debug)]
pub struct RetryPolicy {
    metrics: Option<Handle>,
    budget: Arc<retry::Budget>,
    response_classes: classify::Request,
    max_per_request: Option<NonZeroU32>,
}

/// Allow buffering requests up to 64 kb
const MAX_BUFFERED_BYTES: usize = 64 * 1024;

#[derive(Copy, Clone, Debug)]
pub struct RetryCount(u32);

// === impl NewRetryPolicy ===

impl NewRetryPolicy {
    pub fn new(metrics: metrics::HttpProfileRouteRetry) -> Self {
        Self { metrics }
    }
}

impl<T> retry::NewPolicy<T> for NewRetryPolicy
where
    T: Param<Option<Params>>,
{
    type Policy = RetryPolicy;

    fn new_policy(&self, target: &T) -> Option<Self::Policy> {
        let Params {
            budget,
            max_per_request,
            profile_labels,
            response_classes,
        } = Param::<Option<Params>>::param(target)?;
        let metrics = profile_labels.map(|labels| self.metrics.get_handle(labels));
        Some(RetryPolicy {
            metrics,
            budget,
            response_classes,
            max_per_request,
        })
    }
}

// === impl Retry ===

impl<A, B, E> retry::Policy<http::Request<ReplayBody<A>>, http::Response<WithTrailers<B>>, E>
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
        result: Result<&http::Response<WithTrailers<B>>, &E>,
    ) -> Option<Self::Future> {
        let retryable = match result {
            Err(_) => false,
            Ok(rsp) => {
                // is the request a failure?
                let is_failure = self
                    .response_classes
                    .classify(req)
                    .start(rsp)
                    .eos(rsp.body().trailers())
                    .is_failure();

                // did the body exceed the maximum length limit?
                let exceeded_max_len = req.body().is_capped();

                // was the per-request retry limit exceeded?
                let exceeded_max_retries =
                    match (self.max_per_request, req.extensions().get::<RetryCount>()) {
                        (Some(max_retries), Some(RetryCount(retries))) => {
                            retries >= &max_retries.get()
                        }
                        // if `max_retries_per_request` is `None`, we don't have
                        // a per-request retry limit. if the request's
                        // `RetryCount` is `None`, then it was the initial request.
                        _ => false,
                    };

                let retryable = is_failure && !exceeded_max_len && !exceeded_max_retries;

                tracing::trace!(
                    is_failure,
                    exceeded_max_len,
                    exceeded_max_retries,
                    retryable
                );
                retryable
            }
        };

        if !retryable {
            self.budget.deposit();
            return None;
        }

        let withdrew = self.budget.withdraw().is_ok();
        if let Some(ref metrics) = self.metrics {
            metrics.incr_retryable(withdrew);
        }
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

        // Increment the retry count, if we care about tracking retry counts.
        if self.max_per_request.is_some() {
            let prev = req
                .extensions()
                .get::<RetryCount>()
                .map(|&RetryCount(i)| i)
                // If there's no `retry_count` extension, then this request is
                // the first retry.
                .unwrap_or(0);
            clone.extensions_mut().insert(RetryCount(prev + 1));
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
    B::Error: Unpin + Send,
{
    type RetryRequest = http::Request<ReplayBody<A>>;
    type RetryResponse = http::Response<WithTrailers<B>>;
    type ResponseFuture = future::Map<
        with_trailers::WithTrailersFuture<B>,
        fn(http::Response<WithTrailers<B>>) -> Result<http::Response<WithTrailers<B>>, E>,
    >;

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
        WithTrailers::map_response(rsp).map(Ok)
    }
}
