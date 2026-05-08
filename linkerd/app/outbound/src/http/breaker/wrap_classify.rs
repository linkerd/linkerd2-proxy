//! Per-endpoint gate wiring for the circuit breaker.
//!
//! Each balancer endpoint sits behind a [`svc::Gate`] whose readiness a
//! [`UnifiedBreaker`](super::unified::UnifiedBreaker) controls. The gate is paired with a
//! response classifier that broadcasts each classification over the breaker's channel.
//!
//! When failure accrual is set up for the target, the classifier is wrapped with
//! [`RetryAfterClassify`] so that `Retry-After` and `grpc-retry-pushback-ms` hints are recorded
//! into stores the breaker reads while backing off. The stores are built here, per endpoint, so a
//! hint seen on one endpoint extends only that endpoint's backoff.
//!
//! When accrual is absent, the endpoint uses the stock
//! [`BroadcastClassification`] with no stores and no hint parsing. This matches an
//! endpoint that has no circuit breaking.

use super::retry_after::{GrpcRetryPushbackStore, RetryAfterClassify, RetryAfterStore};
use super::Params;
use linkerd_app_core::{
    classify,
    proxy::http::{self, classify::gate},
    svc::{self, ExtractParam},
    Error,
};
use linkerd_http_classify::BroadcastClassification;
use linkerd_proxy_client_policy::{FailureAccrual, RetryAfterConfig};
use std::{
    task::{Context, Poll},
    time::Duration,
};

/// The classifier the breaker reads when accrual is set up.
type WrappedClassify = RetryAfterClassify<classify::Response>;

/// Parameters for creating the Retry-After aware gate set.
#[derive(Clone, Debug)]
pub struct RetryAfterGateParams {
    pub channel_capacity: usize,
    /// Maximum Retry-After duration to honor. Capped to this value.
    pub max_duration: Duration,
}

/// Reads the per-endpoint failure accrual and Retry-After configuration, then builds the
/// [`NewRetryAfterGate`] that creates each endpoint's gated service.
#[derive(Clone, Debug)]
pub struct NewRetryAfterGateSet<N> {
    inner: N,
    params: RetryAfterGateParams,
}

/// Builds each endpoint's [`svc::Gate`], branching on whether failure accrual is set up.
#[derive(Clone, Debug)]
pub struct NewRetryAfterGate<N> {
    inner: N,
    accrual: Option<FailureAccrual>,
    channel_capacity: usize,
    max_duration: Duration,
}

/// Trait for targets that provide failure accrual configuration.
pub trait HasFailureAccrual {
    fn failure_accrual(&self) -> Option<FailureAccrual>;
}

/// Whether an accrual policy can never trip, which makes it the same as having no circuit breaking.
///
/// Consecutive tracking does nothing when `max_failures` is zero. Success-rate tracking does
/// nothing when it is absent or its threshold is at or below zero, since the EWMA rate stays at or
/// above any such threshold. A policy where both hold could only ever spawn an unused breaker, so
/// it is treated as having no breaker.
fn is_effectively_disabled(accrual: &FailureAccrual) -> bool {
    accrual.consecutive.max_failures == 0
        && accrual.success_rate.is_none_or(|sr| sr.threshold <= 0.0)
}

/// Wraps the request's `classify::Response` with [`RetryAfterClassify`] so the inner broadcaster
/// records hints into the breaker's stores.
///
/// The wrapped classifier is inserted into the request extensions under its own type. The inner
/// [`BroadcastClassification`] reads it from there. The original `classify::Response` extension
/// stays in place for other consumers such as metrics.
#[derive(Clone, Debug)]
pub struct InsertRetryAfterClassify<S> {
    inner: S,
    http_store: RetryAfterStore,
    grpc_store: GrpcRetryPushbackStore,
    max_duration: Duration,
}

// === impl RetryAfterGateParams ===

impl RetryAfterGateParams {
    pub fn new(channel_capacity: usize, max_duration: Duration) -> Self {
        Self {
            channel_capacity,
            max_duration,
        }
    }
}

// === impl NewRetryAfterGateSet ===

impl<N> NewRetryAfterGateSet<N> {
    /// Create a layer that puts a breaker-controlled gate in front of each endpoint.
    pub fn layer(
        params: RetryAfterGateParams,
    ) -> impl svc::layer::Layer<N, Service = Self> + Clone {
        svc::layer::mk(move |inner| Self {
            inner,
            params: params.clone(),
        })
    }
}

impl<T, N> svc::NewService<T> for NewRetryAfterGateSet<N>
where
    T: HasFailureAccrual + svc::Param<Option<RetryAfterConfig>> + Clone,
    N: svc::NewService<T> + Clone,
{
    type Service = NewRetryAfterGate<N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        // Per-target Retry-After cap wins over the stack default.
        let max_duration = svc::Param::<Option<RetryAfterConfig>>::param(&target)
            .map(|ra| ra.max_duration)
            .unwrap_or(self.params.max_duration);

        // An accrual policy that can never trip costs the same as none at all. Treat it as having
        // no breaker so no stores or breaker task get allocated for it.
        let accrual = target
            .failure_accrual()
            .filter(|a| !is_effectively_disabled(a));
        let inner = self.inner.new_service(target);

        NewRetryAfterGate {
            inner,
            accrual,
            channel_capacity: self.params.channel_capacity,
            max_duration,
        }
    }
}

// === impl NewRetryAfterGate ===

impl<T, N, S> svc::NewService<T> for NewRetryAfterGate<N>
where
    N: svc::NewService<T, Service = S>,
    S: svc::Service<
            http::Request<http::BoxBody>,
            Response = http::Response<http::BoxBody>,
            Error = Error,
        > + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Service = svc::Gate<svc::BoxHttp<http::BoxBody>>;

    fn new_service(&self, target: T) -> Self::Service {
        match self.accrual {
            None => {
                // No breaker for this endpoint. Build the stock classifier with a gate that never
                // shuts and no hint stores.
                let (params, _gate_tx, _rsps) =
                    gate::Params::<classify::Class>::channel(self.channel_capacity);
                tracing::trace!("No failure accrual policy enabled.");
                let inner = self.inner.new_service(target);
                let broadcast =
                    BroadcastClassification::<classify::Response, _>::new(params.responses, inner);
                svc::Gate::new(
                    params.gate,
                    svc::BoxHttp::new(http::BoxResponse::new(broadcast)),
                )
            }
            Some(ref accrual) => {
                // Build the stores this endpoint's breaker reads, spawn the breaker, then wrap the
                // classifier so 429 and RESOURCE_EXHAUSTED hints go into those stores.
                let http_store = RetryAfterStore::new();
                let grpc_store = GrpcRetryPushbackStore::new();
                let breaker_params = Params {
                    accrual: Some(accrual.clone()),
                    channel_capacity: self.channel_capacity,
                    retry_after_store: http_store.clone(),
                    grpc_retry_pushback_store: grpc_store.clone(),
                    max_duration: self.max_duration,
                };
                let params: gate::Params<classify::Class> =
                    <Params as ExtractParam<gate::Params<classify::Class>, T>>::extract_param(
                        &breaker_params,
                        &target,
                    );
                let inner = self.inner.new_service(target);
                let broadcast =
                    BroadcastClassification::<WrappedClassify, _>::new(params.responses, inner);
                let wrapped = InsertRetryAfterClassify {
                    inner: broadcast,
                    http_store,
                    grpc_store,
                    max_duration: self.max_duration,
                };
                svc::Gate::new(
                    params.gate,
                    svc::BoxHttp::new(http::BoxResponse::new(wrapped)),
                )
            }
        }
    }
}

// === impl InsertRetryAfterClassify ===

impl<S, RspB> svc::Service<http::Request<http::BoxBody>> for InsertRetryAfterClassify<S>
where
    S: svc::Service<http::Request<http::BoxBody>, Response = http::Response<RspB>, Error = Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = S::Future;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<http::BoxBody>) -> Self::Future {
        if let Some(classify) = req.extensions().get::<classify::Response>().cloned() {
            let wrapped = RetryAfterClassify::new_with_max(
                classify,
                self.http_store.clone(),
                self.grpc_store.clone(),
                self.max_duration,
            );
            req.extensions_mut().insert(wrapped);
        }
        self.inner.call(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linkerd_app_core::{exp_backoff::ExponentialBackoff, svc::NewService};
    use linkerd_http_classify::ClassifyResponse;
    use linkerd_proxy_client_policy::http::StatusRanges;
    use linkerd_proxy_client_policy::{ConsecutiveFailures, SuccessRateConfig};
    use std::time::Duration;

    // `super::*` re-exports `http` (`linkerd_app_core::proxy::http`). Its `Response`, `StatusCode`,
    // `HeaderValue`, and `header` are used to build the test responses below.

    // Helper to create a default HTTP classifier for testing
    fn test_classifier() -> classify::Response {
        classify::Response::Http(StatusRanges::default())
    }

    #[derive(Clone)]
    struct MockTarget(Option<FailureAccrual>);

    impl HasFailureAccrual for MockTarget {
        fn failure_accrual(&self) -> Option<FailureAccrual> {
            self.0.clone()
        }
    }

    impl svc::Param<Option<RetryAfterConfig>> for MockTarget {
        fn param(&self) -> Option<RetryAfterConfig> {
            None
        }
    }

    fn mk_backoff() -> ExponentialBackoff {
        ExponentialBackoff::try_new(Duration::from_secs(1), Duration::from_secs(100), 0.0)
            .expect("backoff params are valid")
    }

    // Read a target through the gate-set layer and report the accrual the per-endpoint gate
    // would see. `None` means the endpoint has no breaker and no added cost.
    fn resolved_accrual(accrual: Option<FailureAccrual>) -> Option<FailureAccrual> {
        let set = NewRetryAfterGateSet {
            inner: |_: MockTarget| (),
            params: RetryAfterGateParams::new(1, Duration::from_secs(300)),
        };
        set.new_service(MockTarget(accrual)).accrual
    }

    // An accrual policy that can never trip is treated the same as a truly absent policy, so it
    // spawns no breaker and allocates no hint stores. This pins the cost of a config that is
    // present but can never trip to the cost of having no circuit breaking at all.
    #[test]
    fn effectively_disabled_accrual_produces_no_breaker_task() {
        let disabled = FailureAccrual {
            consecutive: ConsecutiveFailures {
                max_failures: 0,
                backoff: mk_backoff(),
            },
            success_rate: Some(SuccessRateConfig {
                threshold: 0.0,
                decay: Duration::from_secs(10),
                min_requests: 1,
            }),
        };

        assert!(
            is_effectively_disabled(&disabled),
            "max_failures 0 with a zero success-rate threshold can never trip",
        );
        assert_eq!(
            resolved_accrual(Some(disabled)),
            None,
            "an inert accrual policy must resolve to the disabled path, same as None",
        );

        // A genuinely live policy keeps its enabled resolution.
        let live = FailureAccrual {
            consecutive: ConsecutiveFailures {
                max_failures: 3,
                backoff: mk_backoff(),
            },
            success_rate: None,
        };
        assert!(!is_effectively_disabled(&live));
        assert!(resolved_accrual(Some(live)).is_some());
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn wrapped_classifier_records_retry_after() {
        let http_store = RetryAfterStore::new();
        let grpc_store = GrpcRetryPushbackStore::new();
        let inner = test_classifier();
        let wrapped = RetryAfterClassify::new(inner, http_store.clone(), grpc_store);

        // Create a 429 response with Retry-After header
        let mut rsp = http::Response::new(());
        *rsp.status_mut() = http::StatusCode::TOO_MANY_REQUESTS;
        rsp.headers_mut().insert(
            http::header::RETRY_AFTER,
            http::HeaderValue::from_static("30"),
        );

        // start() should parse and record the hint
        let _eos = wrapped.start(&rsp);

        let hint = http_store.take(Duration::from_secs(10));
        assert_eq!(hint, Some(Duration::from_secs(30)));
    }

    #[test]
    fn wrapped_classifier_ignores_non_429() {
        let http_store = RetryAfterStore::new();
        let grpc_store = GrpcRetryPushbackStore::new();
        let inner = test_classifier();
        let wrapped = RetryAfterClassify::new(inner, http_store.clone(), grpc_store);

        // Create a 200 OK response with Retry-After header (shouldn't be recorded)
        let mut rsp = http::Response::new(());
        *rsp.status_mut() = http::StatusCode::OK;
        rsp.headers_mut().insert(
            http::header::RETRY_AFTER,
            http::HeaderValue::from_static("30"),
        );

        // start() should NOT record the hint
        let _eos = wrapped.start(&rsp);

        // Verify the store is empty
        let hint = http_store.take(Duration::from_secs(10));
        assert_eq!(hint, None);
    }

    // Check that `RetryAfterConfig.max_duration` caps hints through `new_with_max()`.
    //
    // A 429 response with a 600s Retry-After header must be capped to 120s in the store
    // when the classifier was built with `max_duration = 120s`.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn config_max_duration_caps_hint_at_recording() {
        let http_store = RetryAfterStore::new();
        let grpc_store = GrpcRetryPushbackStore::new();
        let inner = test_classifier();
        let custom_cap = Duration::from_secs(120);

        let wrapped =
            RetryAfterClassify::new_with_max(inner, http_store.clone(), grpc_store, custom_cap);

        // Create a 429 response with Retry-After: 600 (exceeds the 120s cap).
        let mut rsp = http::Response::new(());
        *rsp.status_mut() = http::StatusCode::TOO_MANY_REQUESTS;
        rsp.headers_mut().insert(
            http::header::RETRY_AFTER,
            http::HeaderValue::from_static("600"),
        );

        // start() parses the header and records via rate_limit_hint(max_duration).
        let _eos = wrapped.start(&rsp);

        // The store must contain 120s instead of 600s
        let hint = http_store.take(Duration::from_secs(10));
        assert_eq!(
            hint,
            Some(custom_cap),
            "600s Retry-After should be capped to 120s by max_duration at recording site"
        );
    }

    // Check that a hint within the cap is recorded at its actual value (no truncation).
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn config_max_duration_passes_through_uncapped_hint() {
        let http_store = RetryAfterStore::new();
        let grpc_store = GrpcRetryPushbackStore::new();
        let inner = test_classifier();
        let custom_cap = Duration::from_secs(120);

        let wrapped =
            RetryAfterClassify::new_with_max(inner, http_store.clone(), grpc_store, custom_cap);

        // Create a 429 response with Retry-After: 60 (below the 120s cap).
        let mut rsp = http::Response::new(());
        *rsp.status_mut() = http::StatusCode::TOO_MANY_REQUESTS;
        rsp.headers_mut().insert(
            http::header::RETRY_AFTER,
            http::HeaderValue::from_static("60"),
        );

        let _eos = wrapped.start(&rsp);

        // The store must contain the actual 60s value, not the cap.
        let hint = http_store.take(Duration::from_secs(10));
        assert_eq!(
            hint,
            Some(Duration::from_secs(60)),
            "60s Retry-After should pass through unchanged when below 120s cap"
        );
    }
}
