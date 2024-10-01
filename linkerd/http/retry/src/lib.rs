#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

pub mod peek_trailers;
pub mod replay;

pub use self::{peek_trailers::PeekTrailersBody, replay::ReplayBody};
pub use tower::retry::budget::Budget;

use futures::{future, prelude::*};
use linkerd_error::{Error, Result};
use linkerd_exp_backoff::ExponentialBackoff;
use linkerd_http_box::BoxBody;
use linkerd_metrics::prom;
use linkerd_stack::{layer, ExtractParam, NewService, Param, Service};
use std::{
    future::Future,
    hash::Hash,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};
use tower::ServiceExt;
use tracing::{debug, trace};

/// A HTTP retry strategy.
pub trait Policy: Clone + Sized {
    type Future: Future<Output = ()> + Send + 'static;

    /// Determines if a response should be retried.
    fn is_retryable(&self, result: Result<&http::Response<PeekTrailersBody>, &Error>) -> bool;

    /// Prepare headers for the next request.
    fn set_headers(&self, dst: &mut http::HeaderMap, orig: &http::HeaderMap) {
        *dst = orig.clone();
    }

    /// Prepare extensions for the next request.
    fn set_extensions(&self, _dst: &mut http::Extensions, _orig: &http::Extensions) {}
}

#[derive(Clone, Debug)]
pub struct Params {
    pub max_retries: usize,
    pub max_request_bytes: usize,
    pub backoff: Option<ExponentialBackoff>,
}

#[derive(Clone, Debug)]
pub struct NewHttpRetry<P, L: Clone, X, ReqX, N> {
    inner: N,
    metrics: MetricFamilies<L>,
    extract: X,
    _marker: PhantomData<fn() -> (ReqX, P)>,
}

/// A Retry middleware that attempts to extract a `P` typed request extension to
/// instrument retries. When the request extension is not set, requests are not
/// retried.
#[derive(Clone, Debug)]
pub struct HttpRetry<P, L: Clone, ReqX, S> {
    inner: S,
    metrics: MetricFamilies<L>,
    extract: ReqX,
    _marker: PhantomData<fn() -> P>,
}

#[derive(Clone, Debug)]
pub struct MetricFamilies<L: Clone> {
    limit_exceeded: prom::Family<L, prom::Counter>,
    overflow: prom::Family<L, prom::Counter>,
    requests: prom::Family<L, prom::Counter>,
    successes: prom::Family<L, prom::Counter>,
}

#[derive(Clone, Debug, Default)]
struct Metrics {
    requests: prom::Counter,
    successes: prom::Counter,
    limit_exceeded: prom::Counter,
    overflow: prom::Counter,
}

// === impl NewHttpRetry ===

impl<P, L: Clone, X: Clone, ReqX, N> NewHttpRetry<P, L, X, ReqX, N> {
    pub fn layer_via_mk(
        extract: X,
        metrics: MetricFamilies<L>,
    ) -> impl tower::layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self {
            inner,
            extract: extract.clone(),
            metrics: metrics.clone(),
            _marker: PhantomData,
        })
    }
}

impl<T, P, L, X, ReqX, N> NewService<T> for NewHttpRetry<P, L, X, ReqX, N>
where
    P: Policy,
    L: Clone + std::fmt::Debug + Hash + Eq + Send + Sync + prom::encoding::EncodeLabelSet + 'static,
    X: Clone + ExtractParam<ReqX, T>,
    N: NewService<T>,
{
    type Service = HttpRetry<P, L, ReqX, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let Self {
            inner,
            metrics,
            extract,
            _marker,
        } = self;

        let metrics = metrics.clone();
        let extract = extract.extract_param(&target);
        let svc = inner.new_service(target);

        HttpRetry {
            inner: svc,
            metrics,
            extract,
            _marker: PhantomData,
        }
    }
}

// === impl MetricFamilies ===

impl<L> Default for MetricFamilies<L>
where
    L: Clone + std::fmt::Debug + Hash + Eq + Send + Sync + prom::encoding::EncodeLabelSet + 'static,
{
    fn default() -> Self {
        Self {
            limit_exceeded: prom::Family::default(),
            overflow: prom::Family::default(),
            requests: prom::Family::default(),
            successes: prom::Family::default(),
        }
    }
}

impl<L> MetricFamilies<L>
where
    L: Clone + std::fmt::Debug + Hash + Eq + Send + Sync + prom::encoding::EncodeLabelSet + 'static,
{
    pub fn register(registry: &mut prom::Registry) -> Self {
        let limit_exceeded = prom::Family::default();
        registry.register(
            "limit_exceeded",
            "Retryable requests not sent due to retry limits",
            limit_exceeded.clone(),
        );

        let overflow = prom::Family::default();
        registry.register(
            "overflow",
            "Retryable requests not sent due to circuit breakers",
            overflow.clone(),
        );

        let requests = prom::Family::default();
        registry.register("requests", "Retry requests emitted", requests.clone());

        let successes = prom::Family::default();
        registry.register(
            "successes",
            "Successful responses to retry requests",
            successes.clone(),
        );
        Self {
            limit_exceeded,
            overflow,
            requests,
            successes,
        }
    }

    fn metrics(&self, labels: &L) -> Metrics {
        let requests = (*self.requests.get_or_create(labels)).clone();
        let successes = (*self.successes.get_or_create(labels)).clone();
        let limit_exceeded = (*self.limit_exceeded.get_or_create(labels)).clone();
        let overflow = (*self.overflow.get_or_create(labels)).clone();
        Metrics {
            requests,
            successes,
            limit_exceeded,
            overflow,
        }
    }
}

// === impl HttpRetry ===

impl<P, L, ReqX, S> Service<http::Request<BoxBody>> for HttpRetry<P, L, ReqX, S>
where
    P: Policy,
    P: Param<Params>,
    P: Clone + Send + Sync + std::fmt::Debug + 'static,
    L: Clone + std::fmt::Debug + Hash + Eq + Send + Sync + prom::encoding::EncodeLabelSet + 'static,
    ReqX: ExtractParam<L, http::Request<BoxBody>>,
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

    fn call(&mut self, mut req: http::Request<BoxBody>) -> Self::Future {
        // Retries are configured from request extensions so that they can be
        // configured from both policy and request headers.
        let Some(policy) = req.extensions_mut().remove::<P>() else {
            // If there is no policy, there is no need to retry. This avoids
            // buffering logic in the default case.
            trace!(retryable = false, "Request lacks a retry policy");
            return future::Either::Left(self.inner.call(req));
        };

        // TODO(kate): extract the params, metrics, and labels. in the future, we would like to
        // avoid this middleware needing to know about Prometheus labels.
        let params = policy.param();
        let labels = self.extract.extract_param(&req);
        let metrics = self.metrics.metrics(&labels);

        // Since this request is retryable, we need to setup the request body to
        // be buffered/cloneable. If the request body is too large to be cloned,
        // the retry policy is ignored.
        let req = {
            let (head, body) = req.into_parts();
            match ReplayBody::try_new(body, params.max_request_bytes) {
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
        let svc = std::mem::replace(&mut self.inner, pending);
        let call = send_req_with_retries(svc, req, policy, metrics, params);
        future::Either::Right(Box::pin(call))
    }
}

async fn send_req_with_retries(
    // `svc` must be made ready before calling this function.
    mut svc: impl Service<http::Request<BoxBody>, Response = http::Response<BoxBody>, Error = Error>,
    request: http::Request<ReplayBody>,
    policy: impl Policy,
    metrics: Metrics,
    params: Params,
) -> Result<http::Response<BoxBody>> {
    // Initial request.
    let mut backup = mk_backup(&request, &policy);
    let mut result = send_req(&mut svc, request).await;
    if !policy.is_retryable(result.as_ref()) {
        tracing::trace!("Success on first attempt");
        return result.map(|rsp| rsp.map(BoxBody::new));
    }
    if matches!(backup.body().is_capped(), None | Some(true)) {
        // The body was either too large, or we received an early response
        // before the request body was completed read. We cannot safely
        // attempt to send this request again.
        return result.map(|rsp| rsp.map(BoxBody::new));
    }

    // The response was retryable, so continue trying to dispatch backup
    // requests.
    let mut backoff = params.backoff.map(|b| b.stream());
    for n in 1..=params.max_retries {
        if let Some(backoff) = backoff.as_mut() {
            backoff.next().await;
        }

        // The service must be buffered to be cloneable; so if it's not ready,
        // then a circuit breaker is active and requests will be load shed.
        let Some(svc) = svc.ready().now_or_never().transpose()? else {
            tracing::debug!("Retry overflow; service is not ready");
            metrics.overflow.inc();
            return result.map(|rsp| rsp.map(BoxBody::new));
        };

        tracing::debug!(retry.attempt = n);
        let request = backup;
        backup = mk_backup(&request, &policy);
        metrics.requests.inc();
        result = send_req(svc, request).await;
        if !policy.is_retryable(result.as_ref()) {
            if result.is_ok() {
                metrics.successes.inc();
            }
            tracing::debug!("Retry success");
            return result.map(|rsp| rsp.map(BoxBody::new));
        }
        if matches!(backup.body().is_capped(), None | Some(true)) {
            return result.map(|rsp| rsp.map(BoxBody::new));
        }
    }

    // The result is retryable but we've run out of attempts.
    tracing::debug!("Retry limit exceeded");
    metrics.limit_exceeded.inc();
    result.map(|rsp| rsp.map(BoxBody::new))
}

// Make the request and wait for the response. We proactively poll the
// response body for its next frame to convert the response into a
async fn send_req(
    svc: &mut impl Service<http::Request<BoxBody>, Response = http::Response<BoxBody>, Error = Error>,
    req: http::Request<ReplayBody>,
) -> Result<http::Response<PeekTrailersBody>> {
    svc.call(req.map(BoxBody::new))
        .and_then(|rsp| async move {
            tracing::debug!("Peeking at the response trailers");
            let rsp = PeekTrailersBody::map_response(rsp).await;
            Ok(rsp)
        })
        .await
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
