//! Per-endpoint gate wiring for the circuit breaker.
//!
//! Each balancer endpoint sits behind a [`svc::Gate`] whose readiness a breaker
//! task controls, and the gate is paired with a response classifier that
//! broadcasts each classification over the breaker's channel. The stores and the
//! breaker task are built here once per endpoint, so a hint seen on one endpoint
//! extends only that endpoint's backoff and one endpoint's failures never gate
//! another.
//!
//! When failure accrual is set up for the target and the policy respects server
//! hints, the classifier is wrapped with [`RetryAfterClassify`] so that
//! `Retry-After` and `grpc-retry-pushback-ms` hints are recorded into the stores
//! the breaker reads while backing off. A breaker that does not respect hints
//! never reads those stores, so its endpoint skips the recording wrapper and uses
//! the plain [`BroadcastClassification`]. When accrual is absent or inert, the
//! endpoint also uses the plain classifier with no stores and no hint parsing, so
//! it behaves like an endpoint that has no circuit breaking at all.

use super::retry_after::{GrpcRetryPushbackStore, RetryAfterClassify, RetryAfterStore};
use super::Params;
use linkerd_app_core::{
    classify,
    proxy::http::{self, classify::gate},
    svc::{self, ExtractParam},
    Error,
};
use linkerd_http_classify::BroadcastClassification;
use linkerd_proxy_client_policy::FailureAccrual;
use std::{
    task::{Context, Poll},
    time::Duration,
};

/// The classifier the breaker reads when accrual is set up.
type WrappedClassify = RetryAfterClassify<classify::Response>;

/// Parameters for creating the Retry-After aware gate set.
///
/// The recording cap a hint is clamped to is derived per endpoint from the
/// chosen policy's maximum backoff, so it is not held here.
#[derive(Clone, Debug)]
pub struct RetryAfterGateParams {
    pub channel_capacity: usize,
}

/// Reads the per-endpoint failure accrual, then builds the [`NewRetryAfterGate`]
/// that creates each endpoint's gated service.
#[derive(Clone, Debug)]
pub struct NewRetryAfterGateSet<N> {
    inner: N,
    params: RetryAfterGateParams,
}

/// Builds each endpoint's [`svc::Gate`], branching on whether failure accrual is
/// set up.
#[derive(Clone, Debug)]
pub struct NewRetryAfterGate<N> {
    inner: N,
    accrual: Option<FailureAccrual>,
    channel_capacity: usize,
    max_duration: Duration,
}

/// Targets that have a failure accrual policy.
///
/// A target with no policy returns `None`, which selects the no-breaker branch.
pub trait HasFailureAccrual {
    fn failure_accrual(&self) -> Option<FailureAccrual>;
}

/// Whether an accrual policy can never trip, which makes it equivalent to having
/// no circuit breaking.
///
/// Consecutive tracking does nothing when `max_failures` is zero, and a unified
/// policy is inert only when both of its conditions are off, meaning a zero
/// consecutive-failure ceiling together with a zero success-rate threshold.
/// Either inert case could only ever spawn an unused breaker task, so it is
/// treated as having no breaker and no task or hint stores are allocated for it.
fn is_effectively_disabled(accrual: &FailureAccrual) -> bool {
    match accrual {
        FailureAccrual::Consecutive(cf) => cf.max_failures == 0,
        FailureAccrual::Unified(u) => u.max_consecutive_failures == 0 && u.threshold.is_zero(),
    }
}

/// The probe backoff's maximum, which doubles as the recording cap for hints.
///
/// A hint longer than the maximum backoff can never extend the probe wait past
/// it, since the breaker clamps each hint to its `[min, max]` backoff. Capping
/// at the same ceiling when recording keeps an oversized header from sitting in
/// the store until the breaker discards it.
fn accrual_max_backoff(accrual: &FailureAccrual) -> Duration {
    match accrual {
        FailureAccrual::Consecutive(cf) => cf.backoff.max(),
        FailureAccrual::Unified(u) => u.backoff.max(),
    }
}

/// Whether the policy seeds its probe backoff from a server hint.
///
/// The breaker drains the hint stores only when this is set, so a policy that
/// leaves it off never reads what the recording layer would write. Recording
/// then has no observer and can be skipped, leaving the classifier unwrapped.
fn accrual_respects_retry_after_hint(accrual: &FailureAccrual) -> bool {
    match accrual {
        FailureAccrual::Consecutive(cf) => cf.respect_retry_after_hint,
        FailureAccrual::Unified(u) => u.respect_retry_after_hint,
    }
}

/// Inserts a [`RetryAfterClassify`] into each request so the inner broadcaster
/// classifies through it.
///
/// The classifier is inserted under its own type and the inner
/// [`BroadcastClassification`] reads it from there, while the original
/// `classify::Response` extension stays in place for other consumers such as
/// metrics. Every endpoint inserts a classifier of the same type so the gate has
/// one service type. A hint-respecting breaker inserts a recording classifier
/// that writes into its stores; every other endpoint inserts a disabled one that
/// classifies identically and records nothing.
#[derive(Clone, Debug)]
pub struct InsertRetryAfterClassify<S> {
    inner: S,
    record: Option<RetryAfterRecording>,
}

/// The stores and cap a recording classifier writes into. Held only by an
/// endpoint whose breaker respects server hints.
#[derive(Clone, Debug)]
struct RetryAfterRecording {
    http_store: RetryAfterStore,
    grpc_store: GrpcRetryPushbackStore,
    max_duration: Duration,
}

impl<S> InsertRetryAfterClassify<S> {
    /// Insert a recording classifier that writes hints into the breaker's stores.
    fn recording(
        inner: S,
        http_store: RetryAfterStore,
        grpc_store: GrpcRetryPushbackStore,
        max_duration: Duration,
    ) -> Self {
        Self {
            inner,
            record: Some(RetryAfterRecording {
                http_store,
                grpc_store,
                max_duration,
            }),
        }
    }

    /// Insert a disabled classifier that classifies identically but records no
    /// hints, for an endpoint with no breaker or a breaker that ignores hints.
    fn disabled(inner: S) -> Self {
        Self {
            inner,
            record: None,
        }
    }
}

// === impl RetryAfterGateParams ===

impl RetryAfterGateParams {
    pub fn new(channel_capacity: usize) -> Self {
        Self { channel_capacity }
    }
}

// === impl NewRetryAfterGateSet ===

impl<N> NewRetryAfterGateSet<N> {
    /// Create a layer that puts a breaker-controlled gate in front of each
    /// endpoint.
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
    T: HasFailureAccrual + Clone,
    N: svc::NewService<T> + Clone,
{
    type Service = NewRetryAfterGate<N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        // An accrual policy that can never trip costs the same as none at all.
        // Treat it as having no breaker so no stores or breaker task get
        // allocated for it.
        let accrual = target
            .failure_accrual()
            .filter(|a| !is_effectively_disabled(a));

        // The recording cap follows the chosen policy's maximum backoff, the
        // same ceiling the breaker clamps hints to. With no breaker there is no
        // classifier to wrap and nothing reads this cap, so a zero placeholder
        // stands in.
        let max_duration = accrual
            .as_ref()
            .map(accrual_max_backoff)
            .unwrap_or(Duration::ZERO);

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
    // Every endpoint resolves to the same classifier type so the gate needs no
    // per-endpoint service box and no per-request response box. The classifier is
    // the hint-recording [`WrappedClassify`] on all arms; on an endpoint that has
    // no breaker or whose breaker ignores hints it is disabled, so it delegates
    // classification and parses nothing. The balancer boxes the response body once
    // downstream when it unifies its estimator branches, which is the single box
    // on the default path.
    type Service = svc::Gate<InsertRetryAfterClassify<BroadcastClassification<WrappedClassify, S>>>;

    fn new_service(&self, target: T) -> Self::Service {
        match self.accrual {
            None => {
                // No breaker for this endpoint. The gate never shuts, the
                // classifier is disabled so it parses nothing, and the broadcast
                // channel has no reader.
                let (params, _gate_tx, _rsps) =
                    gate::Params::<classify::Class>::channel(self.channel_capacity);
                tracing::trace!("No failure accrual policy enabled.");
                let inner = self.inner.new_service(target);
                let broadcast =
                    BroadcastClassification::<WrappedClassify, _>::new(params.responses, inner);
                svc::Gate::new(params.gate, InsertRetryAfterClassify::disabled(broadcast))
            }
            Some(ref accrual) => {
                // Build the stores this endpoint's breaker reads and spawn the
                // breaker through the params extractor. The breaker always
                // receives the stores, but whether it ever reads them turns on its
                // opt-in flag, the same flag that decides if the classifier
                // records.
                let http_store = RetryAfterStore::new();
                let grpc_store = GrpcRetryPushbackStore::new();
                let breaker_params = Params {
                    accrual: Some(*accrual),
                    channel_capacity: self.channel_capacity,
                    retry_after_store: http_store.clone(),
                    grpc_retry_pushback_store: grpc_store.clone(),
                };
                let params: gate::Params<classify::Class> =
                    <Params as ExtractParam<gate::Params<classify::Class>, T>>::extract_param(
                        &breaker_params,
                        &target,
                    );
                let inner = self.inner.new_service(target);
                let broadcast =
                    BroadcastClassification::<WrappedClassify, _>::new(params.responses, inner);

                // The breaker drains the hint stores only when it respects server
                // hints. With the flag off nothing reads the stores, so the
                // classifier is disabled and records nothing rather than parsing on
                // every request for an observer that never reads it. Both arms
                // share the gate's single service type, so neither adds a box.
                let insert = if accrual_respects_retry_after_hint(accrual) {
                    InsertRetryAfterClassify::recording(
                        broadcast,
                        http_store,
                        grpc_store,
                        self.max_duration,
                    )
                } else {
                    InsertRetryAfterClassify::disabled(broadcast)
                };
                svc::Gate::new(params.gate, insert)
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
            // A recording endpoint inserts a classifier wired to its stores; every
            // other endpoint inserts a disabled classifier of the same type that
            // classifies identically and records nothing.
            let wrapped = match &self.record {
                Some(rec) => RetryAfterClassify::new_with_max(
                    classify,
                    rec.http_store.clone(),
                    rec.grpc_store.clone(),
                    rec.max_duration,
                ),
                None => RetryAfterClassify::disabled(classify),
            };
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
    use linkerd_proxy_client_policy::{http::StatusRanges, ConsecutiveFailures};
    use std::time::Duration;

    // `super::*` re-exports `http` (`linkerd_app_core::proxy::http`). Its
    // `Response`, `StatusCode`, `HeaderValue`, and `header` build the test
    // responses below.

    fn mk_classifier() -> classify::Response {
        classify::Response::Http(StatusRanges::default())
    }

    #[derive(Clone)]
    struct MockTarget(Option<FailureAccrual>);

    impl HasFailureAccrual for MockTarget {
        fn failure_accrual(&self) -> Option<FailureAccrual> {
            self.0
        }
    }

    fn mk_backoff() -> ExponentialBackoff {
        ExponentialBackoff::try_new(Duration::from_secs(1), Duration::from_secs(100), 0.0)
            .expect("backoff params are valid")
    }

    // Read a target through the gate-set layer and report the accrual the
    // per-endpoint gate would see. `None` means the endpoint has no breaker and
    // no added cost.
    fn resolved_accrual(accrual: Option<FailureAccrual>) -> Option<FailureAccrual> {
        let set = NewRetryAfterGateSet {
            inner: |_: MockTarget| (),
            params: RetryAfterGateParams::new(1),
        };
        set.new_service(MockTarget(accrual)).accrual
    }

    // An accrual policy that can never trip is treated the same as a truly
    // absent policy, so it spawns no breaker and allocates no hint stores. This
    // pins the cost of a config that is present but can never trip to the cost
    // of having no circuit breaking at all.
    #[test]
    fn effectively_disabled_accrual_produces_no_breaker_task() {
        // A consecutive policy with a zero ceiling can never trip.
        let disabled = FailureAccrual::Consecutive(ConsecutiveFailures {
            max_failures: 0,
            backoff: mk_backoff(),
            respect_retry_after_hint: false,
        });
        assert!(
            is_effectively_disabled(&disabled),
            "max_failures 0 can never trip",
        );
        assert_eq!(
            resolved_accrual(Some(disabled)),
            None,
            "an inert accrual policy must resolve to the disabled path, same as having no policy",
        );

        // A genuinely live policy keeps its enabled resolution.
        let live = FailureAccrual::Consecutive(ConsecutiveFailures {
            max_failures: 3,
            backoff: mk_backoff(),
            respect_retry_after_hint: false,
        });
        assert!(!is_effectively_disabled(&live));
        assert!(resolved_accrual(Some(live)).is_some());
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn wrapped_classifier_records_retry_after() {
        let http_store = RetryAfterStore::new();
        let grpc_store = GrpcRetryPushbackStore::new();
        let inner = mk_classifier();
        let wrapped = RetryAfterClassify::new(inner, http_store.clone(), grpc_store);

        // A 429 response with a Retry-After header.
        let mut rsp = http::Response::new(());
        *rsp.status_mut() = http::StatusCode::TOO_MANY_REQUESTS;
        rsp.headers_mut().insert(
            http::header::RETRY_AFTER,
            http::HeaderValue::from_static("30"),
        );

        // start() should parse and record the hint.
        let _eos = wrapped.start(&rsp);

        let hint = http_store.take(Duration::from_secs(10));
        assert_eq!(hint, Some(Duration::from_secs(30)));
    }

    #[test]
    fn wrapped_classifier_ignores_non_429() {
        let http_store = RetryAfterStore::new();
        let grpc_store = GrpcRetryPushbackStore::new();
        let inner = mk_classifier();
        let wrapped = RetryAfterClassify::new(inner, http_store.clone(), grpc_store);

        // A 200 OK response with a Retry-After header must not be recorded.
        let mut rsp = http::Response::new(());
        *rsp.status_mut() = http::StatusCode::OK;
        rsp.headers_mut().insert(
            http::header::RETRY_AFTER,
            http::HeaderValue::from_static("30"),
        );

        let _eos = wrapped.start(&rsp);

        let hint = http_store.take(Duration::from_secs(10));
        assert_eq!(hint, None);
    }

    // The recording cap passed to `new_with_max()` caps hints at the recording
    // site: a 429 with a 600s Retry-After header is capped to 120s when the
    // classifier was built with `max_duration = 120s`.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn config_max_duration_caps_hint_at_recording() {
        let http_store = RetryAfterStore::new();
        let grpc_store = GrpcRetryPushbackStore::new();
        let inner = mk_classifier();
        let custom_cap = Duration::from_secs(120);

        let wrapped =
            RetryAfterClassify::new_with_max(inner, http_store.clone(), grpc_store, custom_cap);

        let mut rsp = http::Response::new(());
        *rsp.status_mut() = http::StatusCode::TOO_MANY_REQUESTS;
        rsp.headers_mut().insert(
            http::header::RETRY_AFTER,
            http::HeaderValue::from_static("600"),
        );

        let _eos = wrapped.start(&rsp);

        let hint = http_store.take(Duration::from_secs(10));
        assert_eq!(
            hint,
            Some(custom_cap),
            "600s Retry-After should be capped to 120s by max_duration at recording site",
        );
    }

    // A hint within the cap is kept at its actual value with no truncation.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn config_max_duration_passes_through_uncapped_hint() {
        let http_store = RetryAfterStore::new();
        let grpc_store = GrpcRetryPushbackStore::new();
        let inner = mk_classifier();
        let custom_cap = Duration::from_secs(120);

        let wrapped =
            RetryAfterClassify::new_with_max(inner, http_store.clone(), grpc_store, custom_cap);

        let mut rsp = http::Response::new(());
        *rsp.status_mut() = http::StatusCode::TOO_MANY_REQUESTS;
        rsp.headers_mut().insert(
            http::header::RETRY_AFTER,
            http::HeaderValue::from_static("60"),
        );

        let _eos = wrapped.start(&rsp);

        let hint = http_store.take(Duration::from_secs(10));
        assert_eq!(
            hint,
            Some(Duration::from_secs(60)),
            "60s Retry-After should pass through unchanged when below 120s cap",
        );
    }
}
