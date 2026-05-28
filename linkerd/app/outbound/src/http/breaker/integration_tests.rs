//! Integration tests for the unified circuit breaker.
//!
//! Tests cover specific breaker behaviors: SR-only trip, gRPC classification,
//! Retry-After hint extension, and backwards-compatibility regression checks.

use super::*;

use linkerd_app_core::{exp_backoff::ExponentialBackoff, svc::ExtractParam};
use linkerd_proxy_client_policy::DEFAULT_RETRY_AFTER_MAX_DURATION;
use std::time::Duration;
use tokio::time;
use tokio_test::{assert_pending, task};
use tonic;

// Sends a probe-success classification directly to the breaker's mpsc,
// bypassing Gate/MockService. The caller should first check is_limited()
// to confirm the breaker is in probation (not draining in backoff).
fn send_probe_success(params: &gate::Params<classify::Class>) {
    params
        .responses
        .try_send(classify::Class::Http(Ok(http::StatusCode::OK)))
        .expect("try_send failed -- bump gate channel capacity");
}

fn make_backoff() -> ExponentialBackoff {
    ExponentialBackoff::try_new(
        Duration::from_secs(1),
        Duration::from_secs(100),
        0.0, // no jitter for deterministic tests
    )
    .expect("backoff params are valid")
}

fn send_class(params: &gate::Params<classify::Class>, class: classify::Class) {
    params.responses.try_send(class).unwrap();
}

fn send_ok(params: &gate::Params<classify::Class>, status: http::StatusCode) {
    send_class(params, classify::Class::Http(Ok(status)));
}

fn send_err(params: &gate::Params<classify::Class>, status: http::StatusCode) {
    send_class(params, classify::Class::Http(Err(status)));
}

fn send_grpc(params: &gate::Params<classify::Class>, code: tonic::Code) {
    let class = if matches!(
        code,
        tonic::Code::Unknown
            | tonic::Code::DeadlineExceeded
            | tonic::Code::PermissionDenied
            | tonic::Code::Internal
            | tonic::Code::Unavailable
            | tonic::Code::DataLoss
    ) {
        classify::Class::Grpc(Err(code))
    } else {
        classify::Class::Grpc(Ok(code))
    };
    send_class(params, class);
}

fn default_test_config(
    gate_tx: gate::Tx,
    rsps: tokio::sync::mpsc::Receiver<classify::Class>,
) -> UnifiedBreakerConfig {
    UnifiedBreakerConfig {
        max_failures: 3,
        threshold: 0.8,
        decay: Duration::from_secs(10),
        backoff: make_backoff(),
        min_requests: 1,
        gate: gate_tx,
        rsps,
        retry_after_store: RetryAfterStore::new(),
        grpc_retry_pushback_store: GrpcRetryPushbackStore::new(),
        max_duration: DEFAULT_RETRY_AFTER_MAX_DURATION,
    }
}

// Verifies that the breaker trips via success rate alone when `max_failures=0`
// disables the consecutive-failure policy. The `should_trip` function skips the
// consecutive check when max_failures is zero, so only EWMA degradation can trip.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn sr_only_trip_with_consecutive_disabled() {
    let _trace = linkerd_tracing::test::trace_init();

    let (params, gate_tx, rsps) = gate::Params::channel(8);

    tokio::spawn(
        UnifiedBreaker::new(UnifiedBreakerConfig {
            max_failures: 0, // consecutive failures disabled
            threshold: 0.5,  // trip when SR drops below 50%
            min_requests: 1, // cold-start passes after 1 request
            ..default_test_config(gate_tx, rsps)
        })
        .run(),
    );

    // Advance past t=0 so the EWMA accepts add() calls (skips when ts <= timestamp).
    time::sleep(Duration::from_millis(1)).await;
    assert!(params.gate.is_open(), "gate should start open");

    // One success to satisfy min_requests and seed the EWMA.
    send_ok(&params, http::StatusCode::OK);
    time::advance(Duration::from_secs(1)).await;

    // 429s degrade the EWMA without touching consecutive failures (max_failures=0
    // means the counter is never updated). With decay=10s and 1s spacing,
    // we need about 7 samples to drop below 0.5.
    for _ in 0..8 {
        send_ok(&params, http::StatusCode::TOO_MANY_REQUESTS);
        time::advance(Duration::from_secs(1)).await;
    }

    assert!(
        params.gate.is_shut(),
        "breaker should trip via SR when consecutive failures are disabled",
    );

    // Wait for backoff (1s base + safety margin). With tokio::spawn, the breaker
    // runs on the runtime and enters probation when the backoff timer fires.
    time::advance(Duration::from_secs(2)).await;

    for _ in 0..5 {
        if params.gate.is_limited() {
            break;
        }
        time::advance(Duration::from_millis(100)).await;
    }
    assert!(
        params.gate.is_limited(),
        "gate should enter probation after backoff",
    );

    // Probe success via the channel (same pattern as send_probe_success).
    send_probe_success(&params);
    time::advance(Duration::from_millis(100)).await;

    assert!(
        params.gate.is_open(),
        "gate should reopen after probe success"
    );
}

// Exercises gRPC response classification through the breaker:
//   - Grpc(Err(_)) is a configured failure (increments consecutive counter AND degrades EWMA)
//   - Grpc(Ok(ResourceExhausted)) is a rate failure (degrades EWMA) but does
//     NOT increment consecutive failures
//   - Grpc(Ok(Ok)) is a success (resets consecutive counter)
//
// The negative assertion on consecutive-failure count after ResourceExhausted is
// the key distinction tested here.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn grpc_classification_through_breaker() {
    let _trace = linkerd_tracing::test::trace_init();

    // Part A: ResourceExhausted must NOT trip via consecutive failures

    let (params_a, gate_tx_a, rsps_a) = gate::Params::channel(1);

    let breaker_a = UnifiedBreaker::new(UnifiedBreakerConfig {
        max_failures: 1, // trip after just 1 consecutive configured failure
        threshold: 0.0,  // SR disabled (threshold 0.0 never trips)
        min_requests: usize::MAX,
        ..default_test_config(gate_tx_a, rsps_a)
    });
    let mut task_a = task::spawn(breaker_a.run());

    assert_pending!(task_a.poll());

    // Send 5 ResourceExhausted responses. Because ResourceExhausted is classified
    // as Grpc(Ok(ResourceExhausted)), it is not a configured failure. The
    // consecutive counter stays at 0 and cannot trip even with max_failures=1.
    for _ in 0..5 {
        send_grpc(&params_a, tonic::Code::ResourceExhausted);
        time::advance(Duration::from_millis(100)).await;
        assert_pending!(task_a.poll());
    }

    assert!(
        params_a.gate.is_open(),
        "ResourceExhausted must not trip consecutive failures (counter should stay at 0)",
    );

    // Part B: gRPC configured failures trip consecutive failures

    let (params_b, gate_tx_b, rsps_b) = gate::Params::channel(1);

    let breaker_b = UnifiedBreaker::new(UnifiedBreakerConfig {
        max_failures: 2,
        threshold: 0.0, // SR disabled
        min_requests: usize::MAX,
        ..default_test_config(gate_tx_b, rsps_b)
    });
    let mut task_b = task::spawn(breaker_b.run());

    assert_pending!(task_b.poll());

    // Unavailable is Grpc(Err(Unavailable)), counting as failure
    send_grpc(&params_b, tonic::Code::Unavailable);
    time::advance(Duration::from_millis(100)).await;
    assert_pending!(task_b.poll());

    send_grpc(&params_b, tonic::Code::Unavailable);
    time::advance(Duration::from_millis(100)).await;
    assert_pending!(task_b.poll());

    assert!(
        params_b.gate.is_shut(),
        "Grpc(Err(Unavailable)) should trip consecutive failures at max_failures=2",
    );

    // Part C: Ok resets the consecutive counter

    let (params_c, gate_tx_c, rsps_c) = gate::Params::channel(1);

    let breaker_c = UnifiedBreaker::new(UnifiedBreakerConfig {
        max_failures: 2,
        threshold: 0.0,
        min_requests: usize::MAX,
        ..default_test_config(gate_tx_c, rsps_c)
    });
    let mut task_c = task::spawn(breaker_c.run());

    assert_pending!(task_c.poll());

    // 1 failure, then Ok resets, then 1 failure again. Should not trip.
    send_grpc(&params_c, tonic::Code::Internal);
    time::advance(Duration::from_millis(100)).await;
    assert_pending!(task_c.poll());

    send_grpc(&params_c, tonic::Code::Ok);
    time::advance(Duration::from_millis(100)).await;
    assert_pending!(task_c.poll());

    send_grpc(&params_c, tonic::Code::Internal);
    time::advance(Duration::from_millis(100)).await;
    assert_pending!(task_c.poll());

    assert!(
        params_c.gate.is_open(),
        "Grpc(Ok(Ok)) should reset consecutive counter, preventing trip at max_failures=2",
    );

    // Part D: ResourceExhausted degrades SR EWMA

    let (params_d, gate_tx_d, rsps_d) = gate::Params::channel(1);

    let breaker_d = UnifiedBreaker::new(UnifiedBreakerConfig {
        max_failures: 100, // consecutive won't trip
        threshold: 0.5,
        min_requests: 1,
        ..default_test_config(gate_tx_d, rsps_d)
    });
    let mut task_d = task::spawn(breaker_d.run());

    assert_pending!(task_d.poll());

    // Seed with one success.
    send_grpc(&params_d, tonic::Code::Ok);
    time::advance(Duration::from_secs(1)).await;
    assert_pending!(task_d.poll());

    // ResourceExhausted pushes EWMA below threshold. With decay=10s and 1s
    // spacing, alpha ~ 0.095 per sample. Need ~7 zeros to drop below 0.5.
    for _ in 0..8 {
        send_grpc(&params_d, tonic::Code::ResourceExhausted);
        time::advance(Duration::from_secs(1)).await;
        assert_pending!(task_d.poll());
    }

    assert!(
        !params_d.gate.is_open(),
        "ResourceExhausted should degrade SR EWMA and trip on low success rate",
    );
}

// Check that a Retry-After hint recorded in the store extends the breaker's
// backoff duration beyond the normal exponential backoff.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn retry_after_hint_extends_backoff_integration() {
    let _trace = linkerd_tracing::test::trace_init();

    let (params, gate_tx, rsps) = gate::Params::channel(1);
    let ra_store = RetryAfterStore::new();

    let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
        retry_after_store: ra_store.clone(),
        ..default_test_config(gate_tx, rsps)
    });
    let mut task = task::spawn(breaker.run());

    assert_pending!(task.poll());

    // Record a 5s Retry-After hint before the trip. The hint must be present
    // in the store when the breaker enters closed state and calls
    // take_combined_hint() on its first backoff iteration. In production the
    // classifier records the hint from the Retry-After header before the
    // breaker loop processes the tripping response.
    ra_store.record(Duration::from_secs(5));

    // Trip via 3 consecutive 5xx
    for _ in 0..3 {
        send_err(&params, http::StatusCode::BAD_GATEWAY);
        time::advance(Duration::from_millis(100)).await;
        assert_pending!(task.poll());
    }
    assert!(params.gate.is_shut(), "breaker should trip after 3x 5xx");

    // Advance 4s (hint minus 1s). The normal 1s exponential backoff would have
    // expired, but the 5s hint keeps the gate shut. This is the important
    // check. Without the hint, probation would arrive at ~1s.
    time::sleep(Duration::from_secs(4)).await;
    assert_pending!(task.poll());
    assert!(
        params.gate.is_shut(),
        "gate should remain shut at 4s (hint is 5s, backoff is 1s)",
    );

    // Advance past the 5s hint boundary. The hint elapses and probation begins.
    time::sleep(Duration::from_secs(1)).await;
    assert_pending!(task.poll());

    assert!(
        params.gate.is_limited(),
        "gate should enter probation after the hint duration expires",
    );
}

// When both stores hold hints longer than the configured maximum, the breaker
// clamps each hint to that maximum before taking the larger of the two. With
// both hints above the cap, the backoff floor is the cap itself, so the gate
// enters probation when the cap elapses rather than when the raw hints would.
// This pins take_combined_hint's clamp-then-max behavior end-to-end.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn combined_hints_clamp_to_max_duration_integration() {
    let _trace = linkerd_tracing::test::trace_init();

    let (params, gate_tx, rsps) = gate::Params::channel(8);
    let ra_store = RetryAfterStore::new();
    let grpc_store = GrpcRetryPushbackStore::new();

    let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
        retry_after_store: ra_store.clone(),
        grpc_retry_pushback_store: grpc_store.clone(),
        ..default_test_config(gate_tx, rsps)
    });
    let mut task = task::spawn(breaker.run());

    assert_pending!(task.poll());

    // Both hints exceed the cap. Each is clamped to max_duration, so the
    // combined floor is max(cap, cap) = cap, not the raw 600s.
    let over_cap = DEFAULT_RETRY_AFTER_MAX_DURATION + Duration::from_secs(300);
    ra_store.record(over_cap);
    grpc_store.record(over_cap);

    // Trip via 3 consecutive 5xx
    for _ in 0..3 {
        send_err(&params, http::StatusCode::BAD_GATEWAY);
        time::advance(Duration::from_millis(100)).await;
        assert_pending!(task.poll());
    }
    assert!(params.gate.is_shut(), "breaker should trip after 3x 5xx");

    // Just short of the cap the gate is still shut. The clamped floor has not
    // elapsed yet.
    time::sleep(DEFAULT_RETRY_AFTER_MAX_DURATION - Duration::from_secs(1)).await;
    assert_pending!(task.poll());
    assert!(
        params.gate.is_shut(),
        "gate should remain shut until the clamped max_duration elapses",
    );

    // Past the cap the gate enters probation. If either raw 600s hint had
    // survived unclamped, the gate would still be shut here.
    time::sleep(Duration::from_secs(2)).await;
    assert_pending!(task.poll());
    assert!(
        params.gate.is_limited(),
        "gate should enter probation once max_duration elapses, proving both hints were clamped",
    );
}

// In consecutive-only mode (success_rate=None), a 429 response during probation
// is evaluated using `class.is_success()` (the old code behavior), not the
// stricter 429-as-failure check used in dual mode. This preserves backwards
// compatibility for people that do not enable success rate tracking.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn consecutive_only_429_does_not_change_probe_behavior() {
    let _trace = linkerd_tracing::test::trace_init();

    let (params, gate_tx, rsps) = gate::Params::channel(1);

    // Consecutive-only mode: success_rate disabled via threshold=0.0 and
    // min_requests=usize::MAX (the sentinel values used when the annotation
    // has no success_rate configuration).
    let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
        max_failures: 2,
        threshold: 0.0,
        min_requests: usize::MAX,
        ..default_test_config(gate_tx, rsps)
    });
    let mut task = task::spawn(breaker.run());

    assert_pending!(task.poll());

    // Trip via 2 consecutive 5xx.
    for _ in 0..2 {
        send_err(&params, http::StatusCode::BAD_GATEWAY);
        time::advance(Duration::from_millis(100)).await;
        assert_pending!(task.poll());
    }
    assert!(params.gate.is_shut(), "breaker should trip after 2x 5xx");

    // Wait for backoff to enter probation.
    time::sleep(Duration::from_secs(2)).await;
    assert_pending!(task.poll());

    for _ in 0..5 {
        if params.gate.is_limited() {
            break;
        }
        time::advance(Duration::from_millis(100)).await;
        assert_pending!(task.poll());
    }
    assert!(
        params.gate.is_limited(),
        "gate should be in probation (Limited)",
    );

    // Send a 429 as the probe response. In consecutive-only mode 429 is
    // evaluated by class.is_success() which treats 429 as success (it is not
    // a server error). This is the old consecutive-failures breaker behavior.
    send_ok(&params, http::StatusCode::TOO_MANY_REQUESTS);
    time::advance(Duration::from_millis(100)).await;
    assert_pending!(task.poll());

    // The breaker should reopen because the consecutive-only probe check
    // delegates to class.is_success(), and 429 is_success() == true.
    assert!(
        params.gate.is_open(),
        "consecutive-only mode: 429 during probation should be treated as success \
         (delegates to class.is_success()), preserving old breaker semantics",
    );
}

// The breaker reads only the stores it holds. A hint recorded into a different
// store never reaches it, so the backoff stays on the base schedule. The gate
// set builds a separate store pair per endpoint, so one breaker cannot read a
// hint meant for another.
//
// The test models this directly. A hint goes into an unrelated store while the
// breaker holds the empty stores from default_test_config. The breaker trips
// and reopens on the base 1s backoff, proving the recorded hint was never read.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn hint_in_unheld_store_is_ignored() {
    let _trace = linkerd_tracing::test::trace_init();

    let (params, gate_tx, rsps) = gate::Params::channel(1);

    // The breaker keeps the empty stores from default_test_config.
    let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
        ..default_test_config(gate_tx, rsps)
    });
    let mut task = task::spawn(breaker.run());

    assert_pending!(task.poll());

    // Record a long hint into a store the breaker does not hold. With the
    // classifier separated from the breaker's stores, this hint cannot
    // change the backoff.
    let unread_store = RetryAfterStore::new();
    unread_store.record(Duration::from_secs(10));

    // Trip via 3 consecutive 5xx
    for _ in 0..3 {
        send_err(&params, http::StatusCode::BAD_GATEWAY);
        time::advance(Duration::from_millis(100)).await;
        assert_pending!(task.poll());
    }
    assert!(params.gate.is_shut(), "breaker should trip after 3x 5xx");

    // Advance just past the 1s base backoff. If the 10s hint had reached the
    // breaker, the gate would still be shut. Because it did not, probation
    // arrives on the base interval.
    time::sleep(Duration::from_millis(1050)).await;
    assert_pending!(task.poll());

    assert!(
        params.gate.is_limited(),
        "gate should enter probation on the base 1s backoff when no hint reaches the breaker",
    );

    // The unrelated store still holds its hint. The breaker never took it.
    // take() adjusts the returned duration for elapsed time, so the exact value
    // is not pinned here. What matters is that the slot is still occupied.
    assert!(
        unread_store.take(Duration::from_secs(60)).is_some(),
        "the hint must remain in the unread store, untouched by the breaker",
    );
}

// When failure_accrual is None (disabled), the ExtractParam impl does
// not spawn a breaker task. The gate stays open permanently with no
// state transitions.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn failure_accrual_none_produces_no_breaker_task() {
    let _trace = linkerd_tracing::test::trace_init();

    let params_struct = Params {
        accrual: None,
        channel_capacity: 1,
        retry_after_store: RetryAfterStore::new(),
        grpc_retry_pushback_store: GrpcRetryPushbackStore::new(),
        max_duration: DEFAULT_RETRY_AFTER_MAX_DURATION,
    };

    let gate_params: gate::Params<classify::Class> = params_struct.extract_param(&());

    // The gate should start open.
    assert!(
        gate_params.gate.is_open(),
        "gate should be open when failure_accrual is None",
    );

    // The response channel receiver was dropped because no breaker task was
    // spawned. try_send() returns an error because nobody is listening.
    // The expected behavior is the channel sender exists in the gate params
    // (for BroadcastClassification to use), but sends are silently dropped.
    let send_result = gate_params.responses.try_send(classify::Class::Http(Err(
        http::StatusCode::INTERNAL_SERVER_ERROR,
    )));
    assert!(
        send_result.is_err(),
        "try_send should fail because no breaker task consumes the receiver",
    );

    // Give the runtime a chance to process any spawned tasks.
    time::sleep(Duration::from_millis(100)).await;

    // The gate must still be open. No breaker task was spawned to shut it.
    assert!(
        gate_params.gate.is_open(),
        "gate should remain permanently open when failure_accrual is None \
         (no breaker task spawned to close it)",
    );
}

// Drives a real 429 with a Retry-After header through the classifier and on
// into the breaker's backoff, end to end. The classifier records into the exact
// RetryAfterStore the breaker drains, so a mismatch between the two store
// instances would show up here as a base-interval backoff instead of the
// extended one. The header value, not a pre-seeded store entry, sets the floor.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn retry_after_header_extends_backoff_end_to_end() {
    use super::retry_after::RetryAfterClassify;
    use linkerd_http_classify::ClassifyResponse;
    use linkerd_proxy_client_policy::http::StatusRanges;

    let _trace = linkerd_tracing::test::trace_init();

    let (params, gate_tx, rsps) = gate::Params::channel(1);
    let ra_store = RetryAfterStore::new();
    let grpc_store = GrpcRetryPushbackStore::new();

    // The breaker drains the very stores the classifier writes to.
    let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
        retry_after_store: ra_store.clone(),
        grpc_retry_pushback_store: grpc_store.clone(),
        ..default_test_config(gate_tx, rsps)
    });
    let mut task = task::spawn(breaker.run());

    assert_pending!(task.poll());

    // Build an actual 429 with `Retry-After: 5` and run it through the
    // classifier bound to the breaker's stores. The classifier parses the
    // header and records 5s into ra_store.
    let classifier = RetryAfterClassify::new(
        classify::Response::Http(StatusRanges::default()),
        ra_store.clone(),
        grpc_store.clone(),
    );
    let mut rsp = http::Response::new(());
    *rsp.status_mut() = http::StatusCode::TOO_MANY_REQUESTS;
    rsp.headers_mut().insert(
        http::header::RETRY_AFTER,
        http::HeaderValue::from_static("5"),
    );
    let _eos = classifier.start(&rsp);

    // Trip via 3 consecutive 5xx. The hint is already in the store when the
    // breaker enters its backoff and calls take_combined_hint().
    for _ in 0..3 {
        send_err(&params, http::StatusCode::BAD_GATEWAY);
        time::advance(Duration::from_millis(100)).await;
        assert_pending!(task.poll());
    }
    assert!(params.gate.is_shut(), "breaker should trip after 3x 5xx");

    // At 4s the base 1s backoff has long passed, but the 5s header hint (less
    // the ~300ms consumed while tripping) keeps the gate shut. This is the
    // important check. Without the header reaching the breaker's store,
    // probation would have arrived at ~1s.
    time::sleep(Duration::from_secs(4)).await;
    assert_pending!(task.poll());
    assert!(
        params.gate.is_shut(),
        "gate should remain shut at 4s because the Retry-After header extended the backoff",
    );

    // Past the hint window the gate enters probation.
    time::sleep(Duration::from_secs(1)).await;
    assert_pending!(task.poll());
    assert!(
        params.gate.is_limited(),
        "gate should enter probation once the header-derived hint elapses",
    );
}

// The gRPC counterpart. A real 200 OK whose RESOURCE_EXHAUSTED trailer holds
// `grpc-retry-pushback-ms` runs through the classifier's trailer handling into
// the breaker's gRPC store. As with the HTTP case, the classifier and breaker
// share one store instance, so this catches a wiring split that the
// store-injecting tests cannot. The trailer pushback, not a seeded value, drives
// the backoff.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn grpc_pushback_trailer_extends_backoff_end_to_end() {
    use super::retry_after::RetryAfterClassify;
    use linkerd_http_classify::{ClassifyEos, ClassifyResponse};
    use linkerd_proxy_client_policy::grpc::Codes;

    let _trace = linkerd_tracing::test::trace_init();

    let (params, gate_tx, rsps) = gate::Params::channel(1);
    let ra_store = RetryAfterStore::new();
    let grpc_store = GrpcRetryPushbackStore::new();

    let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
        retry_after_store: ra_store.clone(),
        grpc_retry_pushback_store: grpc_store.clone(),
        ..default_test_config(gate_tx, rsps)
    });
    let mut task = task::spawn(breaker.run());

    assert_pending!(task.poll());

    // A 200 OK with no gRPC status header opens a streaming gRPC response. The
    // RESOURCE_EXHAUSTED status and pushback arrive in the trailers, which the
    // classifier parses into the breaker's gRPC store.
    let classifier = RetryAfterClassify::new(
        classify::Response::Grpc(Codes::default()),
        ra_store.clone(),
        grpc_store.clone(),
    );
    let mut rsp = http::Response::new(());
    *rsp.status_mut() = http::StatusCode::OK;
    rsp.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/grpc"),
    );
    let eos = classifier.start(&rsp);

    let mut trailers = http::HeaderMap::new();
    trailers.insert("grpc-status", http::HeaderValue::from_static("8"));
    trailers.insert(
        "grpc-retry-pushback-ms",
        http::HeaderValue::from_static("5000"),
    );
    let class = eos.eos(Some(&trailers));
    assert_eq!(
        class,
        classify::Class::Grpc(Ok(tonic::Code::ResourceExhausted)),
        "trailer classification should yield RESOURCE_EXHAUSTED",
    );

    // Trip via 3 consecutive 5xx. The 5s pushback is in the gRPC store before
    // the breaker reaches its backoff.
    for _ in 0..3 {
        send_err(&params, http::StatusCode::BAD_GATEWAY);
        time::advance(Duration::from_millis(100)).await;
        assert_pending!(task.poll());
    }
    assert!(params.gate.is_shut(), "breaker should trip after 3x 5xx");

    // At 4s the gate is still shut because the 5s trailer pushback (less the
    // ~300ms tripping window) extended the backoff beyond the base 1s.
    time::sleep(Duration::from_secs(4)).await;
    assert_pending!(task.poll());
    assert!(
        params.gate.is_shut(),
        "gate should remain shut at 4s because the gRPC pushback trailer extended the backoff",
    );

    // Past the pushback window the gate enters probation.
    time::sleep(Duration::from_secs(1)).await;
    assert_pending!(task.poll());
    assert!(
        params.gate.is_limited(),
        "gate should enter probation once the trailer-derived pushback elapses",
    );
}
