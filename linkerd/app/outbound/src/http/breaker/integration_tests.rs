//! End-to-end tests for the unified circuit breaker as it is wired into the
//! outbound stack.
//!
//! These tests drive the breaker through the [`Params`] dispatch that the
//! outbound stack uses to spawn a policy for each endpoint. They cover the two
//! trip conditions (the success-rate time window and the consecutive-failure
//! ceiling), recovery through bounded probation, how the two policies probe
//! either strictly or leniently, honoring a server pushback hint as a backoff
//! floor, and the per-endpoint isolation of those hints. The engine's internal
//! state machine is unit-tested in `unified`, so what these tests exercise is the
//! wiring that turns a [`FailureAccrual`] policy into a running breaker.

use super::*;

use linkerd_app_core::{exp_backoff::ExponentialBackoff, svc::ExtractParam};
use linkerd_proxy_client_policy::{
    ConsecutiveFailures, FailureAccrual, SuccessRateThreshold, Unified,
};
use std::time::Duration;
use tokio::time;

/// Base probe backoff: a 1s floor and a 100s ceiling, no jitter so the timing
/// assertions are deterministic. A recorded hint is clamped to the ceiling.
const TEST_BASE_BACKOFF: Duration = Duration::from_secs(1);
const TEST_MAX_BACKOFF: Duration = Duration::from_secs(100);

/// Success-rate measurement window. With ten buckets this yields a 1s bucket
/// width, so a request and the timer advance that follows it land in distinct
/// buckets and the windowed ratio is easy to reason about.
const TEST_WINDOW: Duration = Duration::from_secs(10);

fn make_backoff() -> ExponentialBackoff {
    ExponentialBackoff::try_new(TEST_BASE_BACKOFF, TEST_MAX_BACKOFF, 0.0)
        .expect("backoff params are valid")
}

/// A consecutive-failures policy: trip after `max_failures` consecutive
/// failures.
fn consecutive_accrual(max_failures: usize) -> FailureAccrual {
    FailureAccrual::Consecutive(ConsecutiveFailures {
        max_failures,
        backoff: make_backoff(),
    })
}

/// A unified policy: a consecutive-failure ceiling plus a success-rate threshold
/// over the trailing window, either of which trips. The probe is strict.
fn unified_accrual(
    threshold: f64,
    min_requests: u32,
    max_consecutive_failures: usize,
    respect_retry_after_hint: bool,
) -> FailureAccrual {
    FailureAccrual::Unified(Unified {
        threshold: SuccessRateThreshold::from_fraction(threshold),
        window: TEST_WINDOW,
        min_requests,
        max_consecutive_failures,
        backoff: make_backoff(),
        respect_retry_after_hint,
    })
}

/// Build the breaker dispatch params for one endpoint with fresh hint stores.
///
/// Each call mints a distinct store pair, modeling the per-endpoint stores the
/// stack builds so a hint seen on one endpoint cannot reach another's breaker.
fn endpoint_params(accrual: Option<FailureAccrual>) -> Params {
    Params {
        accrual,
        channel_capacity: 8,
    }
}

fn send_class(gate_params: &gate::Params<classify::Class>, class: classify::Class) {
    gate_params
        .responses
        .try_send(class)
        .expect("try_send failed -- bump the gate channel capacity");
}

fn send_ok(gate_params: &gate::Params<classify::Class>, status: http::StatusCode) {
    send_class(gate_params, classify::Class::Http(Ok(status)));
}

fn send_err(gate_params: &gate::Params<classify::Class>, status: http::StatusCode) {
    send_class(gate_params, classify::Class::Http(Err(status)));
}

/// Feed `count` 5xx responses, advancing a small step after each so the spawned
/// breaker task observes them.
async fn fail(gate_params: &gate::Params<classify::Class>, count: usize) {
    for _ in 0..count {
        send_err(gate_params, http::StatusCode::BAD_GATEWAY);
        time::advance(Duration::from_millis(100)).await;
    }
}

/// Advance time until the gate enters probation (Limited), or panic. The
/// dispatched engine runs on the runtime, so the test advances time and polls
/// the gate rather than driving a task handle.
async fn advance_to_probation(gate: &gate::Rx) {
    for _ in 0..50 {
        if gate.is_limited() {
            return;
        }
        time::advance(Duration::from_millis(100)).await;
    }
    panic!("gate did not enter probation");
}

/// Advance time until the gate reopens (Open), or panic.
async fn advance_to_open(gate: &gate::Rx) {
    for _ in 0..20 {
        if gate.is_open() {
            return;
        }
        time::advance(Duration::from_millis(50)).await;
    }
    panic!("gate did not reopen");
}

// The unified policy trips on a run of consecutive 5xx even before the success-rate
// window has the samples to act. The consecutive ceiling does not wait for any
// warm-up, so a hard outage opens the circuit at once. Here the success-rate
// dimension stays dormant (min_requests above the failure count) so the trip is
// attributable to the consecutive ceiling alone.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn unified_trips_on_consecutive_failures() {
    let _trace = linkerd_tracing::test::trace_init();

    let params = endpoint_params(Some(unified_accrual(0.8, 100, 3, true)));
    let gate_params: gate::Params<classify::Class> = params.extract_param(&());

    time::advance(Duration::from_millis(1)).await;
    assert!(gate_params.gate.is_open(), "gate starts open");

    // Two 5xx are not enough, since the ceiling is three.
    fail(&gate_params, 2).await;
    assert!(
        gate_params.gate.is_open(),
        "two 5xx must not trip the three-failure ceiling",
    );

    // The third consecutive 5xx crosses the ceiling.
    fail(&gate_params, 1).await;
    assert!(
        gate_params.gate.is_shut(),
        "three consecutive 5xx trip the unified breaker on the consecutive ceiling",
    );
}

// The unified policy trips on the windowed success rate when no run of consecutive
// failures forms. Rate-limit signals do not count toward the consecutive
// ceiling, so a stream of 429s can only trip the circuit through the
// success-rate dimension. The window is an exact-count ring, so the ratio falls
// below the threshold on the first in-window failure once the sample floor is
// met, independent of request rate.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn unified_trips_on_low_success_rate() {
    let _trace = linkerd_tracing::test::trace_init();

    // Consecutive ceiling high enough to stay out of the way, while a single
    // in-window sample satisfies the floor.
    let params = endpoint_params(Some(unified_accrual(0.8, 1, 100, true)));
    let gate_params: gate::Params<classify::Class> = params.extract_param(&());

    time::advance(Duration::from_millis(1)).await;
    assert!(gate_params.gate.is_open(), "gate starts open");

    // One success seeds the window above the threshold.
    send_ok(&gate_params, http::StatusCode::OK);
    time::advance(Duration::from_secs(1)).await;
    assert!(
        gate_params.gate.is_open(),
        "a single success keeps the ratio above the threshold",
    );

    // 429s drive the windowed ratio under the threshold without ever forming a
    // run of consecutive 5xx.
    for _ in 0..4 {
        send_ok(&gate_params, http::StatusCode::TOO_MANY_REQUESTS);
        time::advance(Duration::from_secs(1)).await;
    }

    assert!(
        gate_params.gate.is_shut(),
        "a low windowed success rate trips the unified breaker without a consecutive run",
    );
}

// Recovery through bounded probation: after the backoff the breaker admits one
// probe, and a successful probe reopens the circuit. The probe verdict is
// delivered as the next classification once the gate is Limited.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn unified_probe_success_reopens() {
    let _trace = linkerd_tracing::test::trace_init();

    // Trip on the consecutive ceiling (success-rate dimension held dormant by a
    // high sample floor) so the two failures trip cleanly and the next
    // classification is unambiguously the probe verdict.
    let params = endpoint_params(Some(unified_accrual(0.8, 100, 2, true)));
    let gate_params: gate::Params<classify::Class> = params.extract_param(&());

    time::advance(Duration::from_millis(1)).await;

    fail(&gate_params, 2).await;
    assert!(
        gate_params.gate.is_shut(),
        "two 5xx trip the unified breaker"
    );

    advance_to_probation(&gate_params.gate).await;

    // A clean probe reopens the circuit.
    send_ok(&gate_params, http::StatusCode::OK);
    advance_to_open(&gate_params.gate).await;
    assert!(
        gate_params.gate.is_open(),
        "a successful probe reopens the unified breaker",
    );
}

// A failed probe re-shuts the circuit and the backoff advances. After the first
// probe fails the next probation does not arrive at the base interval. The
// breaker is waiting out the escalated backoff, so the gate is still shut once
// the base interval elapses.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn unified_failed_probe_advances_backoff() {
    let _trace = linkerd_tracing::test::trace_init();

    // Consecutive-ceiling trip with the success-rate dimension dormant, so the
    // probe slot is clean and the escalation is attributable to the failed probe.
    let params = endpoint_params(Some(unified_accrual(0.8, 100, 2, true)));
    let gate_params: gate::Params<classify::Class> = params.extract_param(&());

    time::advance(Duration::from_millis(1)).await;

    fail(&gate_params, 2).await;
    assert!(
        gate_params.gate.is_shut(),
        "two 5xx trip the unified breaker"
    );

    advance_to_probation(&gate_params.gate).await;

    // The probe fails with another 5xx, so the circuit re-shuts.
    send_err(&gate_params, http::StatusCode::BAD_GATEWAY);
    for _ in 0..10 {
        if gate_params.gate.is_shut() {
            break;
        }
        time::advance(Duration::from_millis(50)).await;
    }
    assert!(
        gate_params.gate.is_shut(),
        "a failed probe re-shuts the circuit",
    );

    // The backoff has escalated past the base interval. After the base 1s the
    // gate is still shut, and had the backoff reset, probation would already be
    // available.
    time::advance(TEST_BASE_BACKOFF + Duration::from_millis(100)).await;
    assert!(
        !gate_params.gate.is_limited(),
        "after a failed probe the backoff advances beyond the base interval",
    );

    // The escalated backoff does eventually admit another probe.
    advance_to_probation(&gate_params.gate).await;
    assert!(
        gate_params.gate.is_limited(),
        "the escalated backoff eventually admits another probe",
    );
}

// A timed-out probe is treated as a failure. If a probe never produces a
// classification within the probe window, the breaker re-shuts and keeps backing
// off rather than hanging in probation or reopening on no evidence.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn unified_timed_out_probe_re_shuts() {
    let _trace = linkerd_tracing::test::trace_init();

    // Consecutive-ceiling trip with the success-rate dimension dormant, so no
    // stray classification can stand in for the probe the test deliberately
    // withholds.
    let params = endpoint_params(Some(unified_accrual(0.8, 100, 2, true)));
    let gate_params: gate::Params<classify::Class> = params.extract_param(&());

    time::advance(Duration::from_millis(1)).await;

    fail(&gate_params, 2).await;
    assert!(
        gate_params.gate.is_shut(),
        "two 5xx trip the unified breaker"
    );

    advance_to_probation(&gate_params.gate).await;

    // No probe verdict arrives. Once the probe window elapses the breaker treats
    // the silent probe as a failure and re-shuts.
    time::advance(TEST_MAX_BACKOFF).await;
    assert!(
        !gate_params.gate.is_open(),
        "a probe that yields no verdict must not reopen the circuit",
    );
}

// The consecutive policy probes leniently: a 429 during probation reopens the
// circuit since the default classifier treats 429 as success. The strict rule
// that counts 429 as failure belongs to the unified policy, so this pins the
// lenient probe of the consecutive-only policy.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn consecutive_probe_is_lenient() {
    let _trace = linkerd_tracing::test::trace_init();

    let params = endpoint_params(Some(consecutive_accrual(2)));
    let gate_params: gate::Params<classify::Class> = params.extract_param(&());

    time::advance(Duration::from_millis(1)).await;
    assert!(gate_params.gate.is_open(), "gate starts open");

    fail(&gate_params, 2).await;
    assert!(
        gate_params.gate.is_shut(),
        "two 5xx trip the consecutive breaker",
    );

    advance_to_probation(&gate_params.gate).await;

    // A 429 probe reopens under the lenient probe.
    send_ok(&gate_params, http::StatusCode::TOO_MANY_REQUESTS);
    advance_to_open(&gate_params.gate).await;
    assert!(
        gate_params.gate.is_open(),
        "consecutive: a 429 probe reopens via the lenient is_success check",
    );
}

// The unified policy probes strictly: a 429 during probation keeps the circuit shut
// since a rate-limit signal counts as a probe failure. This is the dual to the
// lenient consecutive probe and is the recovery half of the success-rate
// dimension treating 429 as failure.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn unified_probe_is_strict() {
    let _trace = linkerd_tracing::test::trace_init();

    // A non-zero threshold keeps the strict probe in force, while a high sample
    // floor keeps the success-rate dimension from tripping, so the two 5xx trip
    // on the consecutive ceiling and the 429 probe exercises the strict path.
    let params = endpoint_params(Some(unified_accrual(0.8, 100, 2, true)));
    let gate_params: gate::Params<classify::Class> = params.extract_param(&());

    time::advance(Duration::from_millis(1)).await;
    assert!(gate_params.gate.is_open(), "gate starts open");

    fail(&gate_params, 2).await;
    assert!(
        gate_params.gate.is_shut(),
        "two 5xx trip the unified breaker"
    );

    advance_to_probation(&gate_params.gate).await;

    // A 429 probe keeps the circuit shut under the strict probe.
    send_ok(&gate_params, http::StatusCode::TOO_MANY_REQUESTS);
    for _ in 0..10 {
        time::advance(Duration::from_millis(50)).await;
    }
    assert!(
        gate_params.gate.is_shut(),
        "unified: a 429 probe keeps the circuit shut under the strict probe",
    );
}

// An absent policy spawns no breaker task. The gate stays open permanently and
// the response receiver is dropped, so classification sends are silently
// discarded. This is the default path for an endpoint with no failure accrual.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn failure_accrual_none_produces_no_breaker_task() {
    let _trace = linkerd_tracing::test::trace_init();

    let params = endpoint_params(None);
    let gate_params: gate::Params<classify::Class> = params.extract_param(&());

    assert!(
        gate_params.gate.is_open(),
        "the gate is open when no failure accrual policy is configured",
    );

    // No breaker task consumes the receiver, so the send is dropped.
    let send_result = gate_params.responses.try_send(classify::Class::Http(Err(
        http::StatusCode::INTERNAL_SERVER_ERROR,
    )));
    assert!(
        send_result.is_err(),
        "with no breaker task the response receiver is dropped and sends fail",
    );

    // No task exists to shut the gate, so it stays open.
    time::advance(Duration::from_millis(100)).await;
    assert!(
        gate_params.gate.is_open(),
        "the gate stays open with no breaker task to close it",
    );
}
