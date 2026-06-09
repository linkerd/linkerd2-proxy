//! Unified circuit breaker combining consecutive failures and success rate policies.
//!
//! This module runs a single circuit breaker with two policies. Consecutive
//! failures trips after N 5xx responses in a row, and success rate trips when
//! the windowed success rate falls below a threshold while counting 429 as a
//! failure.
//!
//! ## Why combine the two signals
//!
//! Each signal alone misses failures that the other catches. A consecutive-failure
//! count reacts at once to a hard outage but stays quiet under a partial one,
//! since an endpoint that fails half its requests never strings together enough
//! failures in a row to trip. A windowed success rate catches that partial
//! degradation. It is slower on a total outage because it must wait for its
//! minimum-sample floor before it can act. Running both in one breaker keeps the
//! fast reaction of the consecutive count together with the partial-failure
//! coverage of the success rate, so either kind of failure trips the circuit.
//!
//! ## Trip Conditions
//!
//! Either condition can trip the circuit. Consecutive failures trips when the
//! count reaches `max_failures`, where 0 turns that dimension off and gives it no
//! protection on first start. Windowed success rate trips when the ratio falls
//! below `threshold` and the window already holds at least `min_requests` samples.
//!
//! Consecutive failures are checked before success rate, so a response that
//! crosses both thresholds at once is charged to [`TripReason::ConsecutiveFailures`].
//! This reason is reported for observability. The circuit trips on either
//! condition the same way.
//!
//! ## Success-Rate Statistic
//!
//! The success-rate dimension counts successes and totals in a ring of
//! fixed-duration buckets that spans the configured window. The ratio is the exact
//! quotient of the live buckets, so it does not depend on request rate. A
//! tight burst of failures and the same burst spread out reach the same decision.
//! See [`SuccessRateWindow`].
//!
//! ## Protection On First Start
//!
//! The success rate policy protects a fresh endpoint in two ways. The circuit
//! cannot trip on low success rate until `min_requests` responses sit in the
//! window, since a small sample is not enough evidence to act on. The window is
//! also self-expiring, so an idle endpoint's buckets age out on their own. After
//! a quiet gap the window is empty, and `min_requests` fresh samples are needed
//! again before the policy can trip. No separate idle timer is required.
//!
//! This protection does not apply to the consecutive failures policy. An
//! endpoint that returns `max_failures` 5xx errors in a row trips at once,
//! since repeated failures are a strong signal regardless of sample size.
//!
//! ## Recovery Conditions
//!
//! During probation a probe counts as recovery only when it is not a 5xx and not
//! a 429 for HTTP, or anything other than RESOURCE_EXHAUSTED for gRPC. A 429
//! during probation counts as a failure, since it shows the endpoint is still
//! rate limiting and the circuit must not reopen while that is the case.
//!
//! ## Retry-After Integration
//!
//! When a 429 or 503 response has a `Retry-After` header the duration is
//! stored in a [`RetryAfterStore`] and used as a backoff floor. While the circuit is
//! tripped the store is consulted before each backoff wait, so a longer hint
//! sent by a later probe raises the floor for the waits that follow.
//!
//! A gRPC RESOURCE_EXHAUSTED response with a `grpc-retry-pushback-ms` header
//! or trailer is stored in a [`GrpcRetryPushbackStore`] and used the same way, and
//! when both an HTTP and a gRPC hint exist the larger wins. Both hints are clamped
//! to the probe backoff's bounds in [`take_combined_hint`], so a single response
//! can neither shorten the base backoff nor push past the escalation ceiling.
//!
//! ## gRPC RESOURCE_EXHAUSTED Handling
//!
//! gRPC's RESOURCE_EXHAUSTED (code 8) is treated like HTTP 429. It counts as a
//! failure for the windowed success rate. It does not count toward consecutive
//! failures because it is not a hard failure. During a probe only gRPC Ok
//! (code 0) counts as success.


use super::{
    success_rate::SuccessRateWindow,
    TripReason,
};
use futures::stream::StreamExt;
use linkerd_app_core::proxy::http::classify::gate;
use linkerd_app_core::{classify, exp_backoff::ExponentialBackoff};
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};
use tonic::Code;

/// Unified circuit breaker combining consecutive failures and success rate policies.
///
/// This policy tracks both consecutive failures from 5xx responses and the
/// windowed success rate that counts both 5xx and 429 as failures, and either
/// condition can trip the circuit. The success rate dimension is protected on
/// first start while the consecutive dimension is not.
pub struct UnifiedBreaker {
    // === Configuration ===
    /// Maximum consecutive failures (5xx only) before a trip. Zero disables this
    /// dimension.
    max_failures: usize,
    /// Success rate threshold in `[0.0, 1.0]`: trip when the windowed ratio drops
    /// below this. A threshold of zero disables the success-rate dimension.
    threshold: f64,
    /// Length of the trailing window over which the success ratio is computed.
    window: Duration,
    /// Exponential backoff schedule for recovery.
    backoff: ExponentialBackoff,
    /// Minimum requests in the window before the circuit can trip on success
    /// rate (cold-start protection on first start).
    min_requests: usize,

    // === Gate Control ===
    /// Gate channel controlling endpoint readiness.
    gate: gate::Tx,
    /// Channel receiving response classifications.
    rsps: mpsc::Receiver<classify::Class>,

    // === Retry-After Integration ===
    /// Whether to honor server Retry-After/pushback hints as a backoff floor.
    /// When false the breaker ignores both stores and follows its own backoff
    /// schedule, so the hint is strictly opt-in.
    respect_retry_after_hint: bool,

    /// Lets tests see the reason charged to each trip without affecting behavior.
    #[cfg(test)]
    trip_observer: Option<mpsc::UnboundedSender<TripReason>>,
}

/// Internal state tracked during the open phase.
struct OpenState {
    /// Current consecutive failure count (5xx only).
    consecutive_failures: usize,
    /// Windowed success ratio tracker where 5xx and 429 both count as failures.
    ///
    /// This is present only when the success-rate dimension is active, which means
    /// a non-zero threshold. A zero threshold leaves it `None`, so no
    /// per-response bookkeeping or trip computation runs for that dimension.
    success_rate: Option<SuccessRateWindow>,
}

impl OpenState {
    /// Create new open state, building the success-rate window only when that
    /// dimension is active.
    ///
    /// The window is empty at construction, so a freshly opened or recovered
    /// endpoint starts with no samples and cannot trip on success rate until
    /// `min_requests` responses build up. This is the optimistic start a new
    /// endpoint should have. There is no seeded "assume healthy" value to decay away.
    fn new(active: bool, window: Duration, now: Instant) -> Self {
        Self {
            consecutive_failures: 0,
            success_rate: active.then(|| SuccessRateWindow::new(window, now)),
        }
    }

    /// Return the state to its initial values after recovery. Clearing the
    /// window starts cold again, so the reopened endpoint must rebuild evidence
    /// before the success-rate dimension can trip again.
    fn reset(&mut self, now: Instant) {
        self.consecutive_failures = 0;
        if let Some(window) = self.success_rate.as_mut() {
            window.reset(now);
        }
    }
}

/// Configuration for [`UnifiedBreaker::new`].
///
/// The engine takes plain primitive parameters, and the config layer reduces its
/// policy types to these before it builds the breaker, so the state machine never
/// depends on how the configuration is represented.
#[derive(Debug)]
pub(crate) struct UnifiedBreakerConfig {
    pub(crate) max_failures: usize,
    pub(crate) threshold: f64,
    pub(crate) window: Duration,
    pub(crate) backoff: ExponentialBackoff,
    pub(crate) min_requests: usize,
    pub(crate) gate: gate::Tx,
    pub(crate) rsps: mpsc::Receiver<classify::Class>,
    pub(crate) respect_retry_after_hint: bool,
}

impl UnifiedBreaker {
    /// Create a new unified circuit breaker.
    pub fn new(config: UnifiedBreakerConfig) -> Self {
        let UnifiedBreakerConfig {
            max_failures,
            threshold,
            window,
            backoff,
            min_requests,
            gate,
            rsps,
            respect_retry_after_hint,
        } = config;
        Self {
            max_failures,
            threshold,
            window,
            backoff,
            min_requests,
            gate,
            rsps,
            respect_retry_after_hint,
            #[cfg(test)]
            trip_observer: None,
        }
    }

    /// Attach a channel that receives the reason for each trip.
    ///
    /// Tests use this to check which condition the breaker charged a trip to.
    #[cfg(test)]
    fn with_trip_observer(mut self, tx: mpsc::UnboundedSender<TripReason>) -> Self {
        self.trip_observer = Some(tx);
        self
    }

    /// Whether the success-rate dimension is active, which a zero threshold turns
    /// off the same way the config layer checks its own zero threshold. When it is
    /// inactive the breaker builds no window and skips the success-rate branch of
    /// the trip check, so that dimension costs nothing.
    fn success_rate_active(&self) -> bool {
        self.threshold > 0.0
    }

    /// Run the circuit breaker state machine.
    ///
    /// The state machine cycles through three states. Open accepts requests and
    /// tracks both policies until a trip condition. Shut closes the gate and waits
    /// out the backoff. Probation admits one probe where a success reopens and a
    /// failure shuts the gate again.
    pub async fn run(mut self) {
        let mut state = OpenState::new(self.success_rate_active(), self.window, Instant::now());

        loop {
            // Open state: track failures until a trip condition is met.
            let trip_reason = match self.open(&mut state).await {
                Ok(reason) => reason,
                Err(()) => return, // Gate lost.
            };

            #[cfg(test)]
            if let Some(tx) = self.trip_observer.as_ref() {
                let _ = tx.send(trip_reason);
            }

            tracing::info!(?trip_reason, "Unified circuit breaker tripped");

            // Shut state plus probation: close the gate, wait out the backoff,
            // then probe.
            match self.closed(trip_reason).await {
                Ok(()) => {
                    state.reset(Instant::now());
                    tracing::info!("Unified circuit breaker reopened");
                }
                Err(()) => return, // Gate lost.
            }
        }
    }

    /// Open state: track both policies until either trips.
    ///
    /// Returns the reason for the trip, or `Err(())` if the gate is lost.
    async fn open(&mut self, state: &mut OpenState) -> Result<TripReason, ()> {
        tracing::debug!("Open");

        self.gate.open().map_err(|_| ())?;

        loop {
            let class = tokio::select! {
                rsp = self.rsps.recv() => rsp.ok_or(())?,
                _ = self.gate.lost() => return Err(()),
            };

            let now = Instant::now();

            let is_configured_failure = !class.is_success();
            // A response degrades the ratio when it is a hard failure or a
            // rate-limit signal. The trip side here and the probe side share the
            // same rate-limit notion, so it lives in one place.
            let degrades_success_rate = class.is_failure() || is_rate_limit_signal(&class);

            // Update the consecutive failure count and skip the work when the
            // policy is off. Rate-limit signals like 429 and RESOURCE_EXHAUSTED do
            // not count here because they are soft and must not trip the
            // hard-failure dimension.
            if self.max_failures > 0 {
                if is_configured_failure {
                    state.consecutive_failures = state.consecutive_failures.saturating_add(1);
                } else {
                    state.consecutive_failures = 0;
                }
            }

            // Feed the success-rate window, where configured failures that are
            // usually 5xx and rate-limit signals like 429 and RESOURCE_EXHAUSTED
            // both count against the ratio. The window exists only when the
            // dimension is active, so a zero-threshold breaker does no work here.
            if let Some(window) = state.success_rate.as_mut() {
                window.record(!degrades_success_rate, now);
                tracing::trace!(
                    ?class,
                    consecutive_failures = state.consecutive_failures,
                    success_rate = ?window.ratio(),
                    min_requests = self.min_requests,
                    "Response"
                );
            } else {
                tracing::trace!(
                    ?class,
                    consecutive_failures = state.consecutive_failures,
                    "Response"
                );
            }

            // Either condition can trip. The consecutive dimension has no first-start
            // protection while the success-rate dimension does. The reported reason
            // reflects whichever condition is checked first.
            if let Some(trip_reason) = self.should_trip(state) {
                return Ok(trip_reason);
            }
        }
    }

    /// Check whether the circuit should trip given the current state.
    ///
    /// Returns the trip reason when a condition is met and `None` otherwise. The
    /// success rate is protected on first start. The consecutive failures have no
    /// such protection.
    ///
    /// Consecutive failures come first, so when a single response crosses both
    /// thresholds at once the reported reason is [`TripReason::ConsecutiveFailures`].
    /// This reason is reported for observability. Either condition trips the
    /// circuit.
    fn should_trip(&self, state: &OpenState) -> Option<TripReason> {
        // A zero max_failures disables the consecutive failure policy entirely.
        if self.max_failures == 0 {
            // Skip the consecutive failures check, since only success rate can trip.
        } else if state.consecutive_failures >= self.max_failures {
            // Consecutive failures crossed the threshold. There is no first-start
            // protection here, so this can trip right after a run of 5xx errors.
            return Some(TripReason::ConsecutiveFailures);
        }

        // Windowed success rate dropped below the threshold. The window is present
        // only when this dimension is active, and its own sample-count guard
        // protects a fresh endpoint. It will not trip until at least
        // `min_requests` responses sit in the window.
        if let Some(window) = state.success_rate.as_ref() {
            if window.should_trip(self.min_requests, self.threshold) {
                return Some(TripReason::LowSuccessRate);
            }
        }

        None
    }

    /// Shut state that backs off, optionally floored by a server hint, then probes.
    ///
    /// It waits out the backoff and then enters probation, where a successful probe
    /// returns `Ok(())` while a failed or timed-out probe resumes the backoff loop.
    /// A probe that never produces a verdict must not hang the loop, so probation is
    /// bounded by the backoff ceiling and a timed-out probe is retried as a failure.
    async fn closed(&mut self, trip_reason: TripReason) -> Result<(), ()> {
        let mut backoff = self.backoff.stream();

        loop {
            // Shut the gate.
            tracing::debug!(backoff = ?backoff.duration(), ?trip_reason, "Shut");
            self.gate.shut().map_err(|_| ())?;

            loop {
                tokio::select! {
                    _ = backoff.next() => break,
                    // Ignore responses while the breaker is shut, but
                    // terminate if the channel is closed. recv() on a
                    // closed channel returns None immediately, which would
                    // otherwise busy-spin this loop.
                    rsp = self.rsps.recv() => { rsp.ok_or(())?; },
                    _ = self.gate.lost() => return Err(()),
                }
            }

            let class = self.probation().await?;
            tracing::trace!(?class, "Response");
            if class.is_success() {
                // Open!
                return Ok(());
            }
        }
    }

    /// Check whether a probe response indicates recovery.
    ///
    /// A probe counts as recovery only when it is not a 5xx and not a 429 for
    /// HTTP, and not RESOURCE_EXHAUSTED for gRPC. A 429 or RESOURCE_EXHAUSTED
    /// during probation counts as a failure, since it signals the endpoint is
    /// still rate limiting, so the circuit must not reopen while that is the case.
    fn is_probe_success(&self, class: &classify::Class) -> bool {
        // A rate-limit signal counts as a probe failure because it shows the
        // endpoint is still rate limiting, which the success-rate dimension
        // tracks, so it must keep the circuit shut. This is the same notion the
        // trip side uses, read here as its complement.
        if is_rate_limit_signal(class) {
            return false;
        }
        match class {
            // A 429 is already handled above, so the only remaining bar is a 5xx
            // a client policy let through as a non-failure.
            classify::Class::Http(Ok(status)) => !status.is_server_error(),
            classify::Class::Http(Err(_)) => false, // 5xx failure.
            // RESOURCE_EXHAUSTED is handled above, so any other accepted Ok code
            // is a recovered probe.
            classify::Class::Grpc(Ok(_)) => true,
            classify::Class::Grpc(Err(_)) => false,
            _ => class.is_success(),
        }
    }

    /// Wait for a response to determine whether the breaker should be opened.
    async fn probation(&mut self) -> Result<classify::Class, ()> {
        tracing::debug!("Probation");
        let _sem = self.gate.limit(1).map_err(|_| ())?;
        tokio::select! {
            rsp = self.rsps.recv() => rsp.ok_or(()),
            _ = self.gate.lost() => Err(()),
        }
    }
}

/// Whether a classified response is a rate-limit signal: HTTP 429 or its gRPC
/// analogue RESOURCE_EXHAUSTED.
///
/// Both the trip side and the probe side pivot on this notion. A rate-limit
/// signal degrades the success rate and, during probation, keeps the circuit
/// shut, so deriving both from one definition keeps the two sides from drifting
/// apart.
fn is_rate_limit_signal(class: &classify::Class) -> bool {
    match class {
        classify::Class::Http(Ok(status)) => *status == http::StatusCode::TOO_MANY_REQUESTS,
        classify::Class::Grpc(Ok(code)) => *code == Code::ResourceExhausted,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time;
    use tokio_test::{assert_pending, task};

    /// Backoff base used across these tests: 1s base, 100s ceiling, no jitter for
    /// determinism.
    fn make_backoff() -> ExponentialBackoff {
        ExponentialBackoff::try_new(
            time::Duration::from_secs(1),
            time::Duration::from_secs(100),
            0.0,
        )
        .expect("backoff params are valid")
    }

    fn send_class(params: &gate::Params<classify::Class>, class: classify::Class) {
        params.responses.try_send(class).unwrap()
    }

    fn send_ok(params: &gate::Params<classify::Class>, status: http::StatusCode) {
        send_class(params, classify::Class::Http(Ok(status)));
    }

    fn send_err(params: &gate::Params<classify::Class>, status: http::StatusCode) {
        send_class(params, classify::Class::Http(Err(status)));
    }

    fn default_config(
        gate_tx: gate::Tx,
        rsps: mpsc::Receiver<classify::Class>,
    ) -> UnifiedBreakerConfig {
        UnifiedBreakerConfig {
            max_failures: 3,
            threshold: 0.8,
            window: time::Duration::from_secs(10),
            backoff: make_backoff(),
            min_requests: 1,
            gate: gate_tx,
            rsps,
            respect_retry_after_hint: true,
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn starts_open() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            ..default_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());
        assert!(params.gate.is_open());
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn trips_on_consecutive_failures() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);

        // Success rate disabled so this isolates the consecutive failures.
        // With the windowed ratio active, a single 5xx at min_requests=1 would
        // trip on rate before the consecutive counter reached its limit.
        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            threshold: 0.0,
            ..default_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());
        assert!(params.gate.is_open());

        // Three consecutive 5xx should trip.
        for i in 1..=3 {
            send_err(&params, http::StatusCode::BAD_GATEWAY);
            time::advance(time::Duration::from_millis(100)).await;
            assert_pending!(task.poll());

            if i < 3 {
                assert!(
                    params.gate.is_open(),
                    "should still be open after {i} failures"
                );
            }
        }

        assert!(params.gate.is_shut(), "should trip after 3 consecutive 5xx");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn success_resets_consecutive_count() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);

        // Isolate the consecutive counter: with success rate off, only a run of
        // three 5xx can trip, so a success resetting the count is observable.
        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            threshold: 0.0,
            ..default_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // Two failures.
        send_err(&params, http::StatusCode::BAD_GATEWAY);
        time::advance(time::Duration::from_millis(100)).await;
        assert_pending!(task.poll());

        send_err(&params, http::StatusCode::BAD_GATEWAY);
        time::advance(time::Duration::from_millis(100)).await;
        assert_pending!(task.poll());
        assert!(params.gate.is_open());

        // A success resets the count.
        send_ok(&params, http::StatusCode::OK);
        time::advance(time::Duration::from_millis(100)).await;
        assert_pending!(task.poll());

        // Two more failures: still need three consecutive.
        send_err(&params, http::StatusCode::BAD_GATEWAY);
        time::advance(time::Duration::from_millis(100)).await;
        assert_pending!(task.poll());

        send_err(&params, http::StatusCode::BAD_GATEWAY);
        time::advance(time::Duration::from_millis(100)).await;
        assert_pending!(task.poll());

        assert!(
            params.gate.is_open(),
            "should still be open: only 2 consecutive after reset"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn trips_on_low_success_rate() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            max_failures: 100, // High so consecutive failures does not trip.
            ..default_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // One success.
        send_ok(&params, http::StatusCode::OK);
        time::advance(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());

        // 429s degrade the windowed ratio. After one success and one 429 the
        // window holds 1 of 2 successes, 0.5, already below the 0.8 threshold, so
        // the circuit trips well inside this loop.
        for _ in 0..4 {
            send_ok(&params, http::StatusCode::TOO_MANY_REQUESTS);
            time::advance(time::Duration::from_secs(1)).await;
            assert_pending!(task.poll());
        }

        assert!(!params.gate.is_open(), "should trip on low success rate");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn success_rate_counts_5xx() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);

        // High max_failures keeps the consecutive policy out of the way. With a
        // 0.8 threshold and a one-sample floor, a single 5xx puts the windowed
        // ratio at 0/1, below the threshold, and trips on rate alone.
        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            max_failures: 100,
            ..default_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // One 5xx is enough: it counts as a failure for the success-rate window.
        send_err(&params, http::StatusCode::BAD_GATEWAY);
        time::advance(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());

        assert!(
            params.gate.is_shut(),
            "5xx should count as failure for success rate"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn cold_start_protects_success_rate() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            max_failures: 100,
            min_requests: 5,
            ..default_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // Four 429s should not trip: only four requests, the floor is five.
        for _ in 0..4 {
            send_ok(&params, http::StatusCode::TOO_MANY_REQUESTS);
            time::advance(time::Duration::from_secs(1)).await;
            assert_pending!(task.poll());
        }

        assert!(
            params.gate.is_open(),
            "cold-start should protect success rate"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn probation_success_reopens() {
        let _trace = linkerd_tracing::test::trace_init();

        let (mut params, gate_tx, rsps) = gate::Params::channel(1);

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            ..default_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // Trip the breaker.
        for _ in 0..3 {
            send_err(&params, http::StatusCode::BAD_GATEWAY);
            time::advance(time::Duration::from_millis(100)).await;
            assert_pending!(task.poll());
        }
        assert!(params.gate.is_shut());

        // Wait out the backoff.
        time::sleep(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());

        // Enter probation.
        match params.gate.state() {
            gate::State::Limited(sem) => {
                assert_eq!(sem.available_permits(), 1);
                params.gate.opened_for_test().await.unwrap().forget();
            }
            _ => panic!("Expected Limited state"),
        }

        // The probe succeeds.
        send_ok(&params, http::StatusCode::OK);
        assert_pending!(task.poll());
        assert!(params.gate.is_open());
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn probation_failure_on_429() {
        let _trace = linkerd_tracing::test::trace_init();

        let (mut params, gate_tx, rsps) = gate::Params::channel(1);

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            ..default_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // Trip the breaker.
        for _ in 0..3 {
            send_err(&params, http::StatusCode::BAD_GATEWAY);
            time::advance(time::Duration::from_millis(100)).await;
            assert_pending!(task.poll());
        }
        assert!(params.gate.is_shut());

        // Wait out the backoff and enter probation.
        time::sleep(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());

        match params.gate.state() {
            gate::State::Limited(_) => {
                params.gate.opened_for_test().await.unwrap().forget();
            }
            _ => panic!("Expected Limited state"),
        }

        // The probe "fails" with a 429: a 429 during probation is a probe failure.
        send_ok(&params, http::StatusCode::TOO_MANY_REQUESTS);
        assert_pending!(task.poll());
        assert!(
            params.gate.is_shut(),
            "429 during probation should fail the probe"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn retry_after_hint_extends_backoff() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            ..default_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // Trip the breaker.
        for _ in 0..3 {
            send_err(&params, http::StatusCode::BAD_GATEWAY);
            time::advance(time::Duration::from_millis(100)).await;
            assert_pending!(task.poll());
        }
        assert!(params.gate.is_shut());

        // After 1s (normal backoff would complete) the gate is still shut due to
        // the hint floor.
        time::sleep(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());
        assert!(params.gate.is_shut());

        // After 3s more (4s total) it is still shut.
        time::sleep(time::Duration::from_secs(3)).await;
        assert_pending!(task.poll());
        assert!(params.gate.is_shut());

        // After 1s more (5s total) it enters probation.
        time::sleep(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());
        assert!(params.gate.is_limited());
    }

    // When hints are not respected, a recorded Retry-After is ignored and the
    // breaker follows its own backoff schedule. With opt-out the gate reaches
    // probation at the base backoff regardless of the stored 5s hint.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn retry_after_hint_ignored_when_disabled() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            respect_retry_after_hint: false,
            ..default_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        for _ in 0..3 {
            send_err(&params, http::StatusCode::BAD_GATEWAY);
            time::advance(time::Duration::from_millis(100)).await;
            assert_pending!(task.poll());
        }
        assert!(params.gate.is_shut());

        // The base 1s backoff alone governs recovery: probation begins at 1s even
        // though a longer hint sits in the store.
        time::sleep(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());
        assert!(
            params.gate.is_limited(),
            "an unrespected hint must not extend the backoff",
        );
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

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn consecutive_failures_ignores_grpc_resource_exhausted() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            threshold: 0.1,    // Very low so success rate does not trip.
            min_requests: 100, // High so success rate does not trip.
            ..default_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // RESOURCE_EXHAUSTED must not count as consecutive failures.
        for _ in 0..5 {
            send_grpc(&params, tonic::Code::ResourceExhausted);
            time::advance(time::Duration::from_millis(100)).await;
            assert_pending!(task.poll());
        }

        assert!(
            params.gate.is_open(),
            "gRPC RESOURCE_EXHAUSTED should not trigger a consecutive-failures trip"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn trips_on_low_success_rate_grpc_resource_exhausted() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            max_failures: 100, // High so consecutive failures does not trip.
            ..default_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // One success.
        send_grpc(&params, tonic::Code::Ok);
        time::advance(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());

        // RESOURCE_EXHAUSTED drops the success rate below 80%.
        for _ in 0..4 {
            send_grpc(&params, tonic::Code::ResourceExhausted);
            time::advance(time::Duration::from_secs(1)).await;
            assert_pending!(task.poll());
        }

        assert!(
            !params.gate.is_open(),
            "should trip on low success rate from gRPC RESOURCE_EXHAUSTED"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn trips_on_low_success_rate_grpc_server_error() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            max_failures: 100,
            ..default_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        send_grpc(&params, tonic::Code::Ok);
        time::advance(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());

        for _ in 0..4 {
            send_grpc(&params, tonic::Code::Internal);
            time::advance(time::Duration::from_secs(1)).await;
            assert_pending!(task.poll());
        }

        assert!(
            !params.gate.is_open(),
            "should trip on low success rate from gRPC Internal errors"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn probation_success_on_grpc_ok() {
        let _trace = linkerd_tracing::test::trace_init();

        let (mut params, gate_tx, rsps) = gate::Params::channel(1);

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            ..default_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // Trip the breaker.
        for _ in 0..3 {
            send_err(&params, http::StatusCode::BAD_GATEWAY);
            time::advance(time::Duration::from_millis(100)).await;
            assert_pending!(task.poll());
        }
        assert!(params.gate.is_shut());

        // Wait out the backoff.
        time::sleep(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());

        // Enter probation.
        match params.gate.state() {
            gate::State::Limited(sem) => {
                assert_eq!(sem.available_permits(), 1);
                params.gate.opened_for_test().await.unwrap().forget();
            }
            _ => panic!("Expected Limited state"),
        }

        // The probe succeeds with gRPC Ok.
        send_grpc(&params, tonic::Code::Ok);
        assert_pending!(task.poll());
        assert!(
            params.gate.is_open(),
            "gRPC Ok during probation should succeed"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn consecutive_failures_ignores_429() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            threshold: 0.1,    // Very low so success rate does not trip.
            min_requests: 100, // High so success rate does not trip.
            ..default_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // 429s must not count as consecutive failures.
        for _ in 0..5 {
            send_ok(&params, http::StatusCode::TOO_MANY_REQUESTS);
            time::advance(time::Duration::from_millis(100)).await;
            assert_pending!(task.poll());
        }

        assert!(
            params.gate.is_open(),
            "429s should not trigger a consecutive-failures trip"
        );
    }

    // A zero threshold disables success-rate tripping.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn threshold_zero_disables_success_rate() {
        let _trace = linkerd_tracing::test::trace_init();
        let (params, gate_tx, rsps) = gate::Params::channel(1);

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            max_failures: 100, // High so consecutive failures does not trip.
            threshold: 0.0,    // Success rate disabled.
            min_requests: 1,
            ..default_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());
        assert!(params.gate.is_open());

        // One success to pass the first-start floor.
        send_ok(&params, http::StatusCode::OK);
        time::advance(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());

        // Many 429s drag the ratio toward zero.
        for _ in 0..10 {
            send_ok(&params, http::StatusCode::TOO_MANY_REQUESTS);
            time::advance(time::Duration::from_secs(1)).await;
            assert_pending!(task.poll());
        }

        assert!(
            params.gate.is_open(),
            "threshold=0.0 should disable success-rate tripping"
        );
    }

    // A zero threshold leaves the success-rate window unbuilt, so OpenState
    // holds no window. A non-zero threshold builds one. This pins, at the state
    // level, that a disabled dimension allocates nothing.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn open_state_window_gated_on_threshold() {
        let now = Instant::now();
        let off = OpenState::new(false, Duration::from_secs(10), now);
        assert!(
            off.success_rate.is_none(),
            "a disabled success-rate dimension builds no window",
        );
        let on = OpenState::new(true, Duration::from_secs(10), now);
        assert!(
            on.success_rate.is_some(),
            "an active success-rate dimension builds a window",
        );
    }

    // With min_requests=0 the success-rate dimension trips on the first failing
    // in-window sample. The zero floor is safe: the window's own emptiness guard
    // keeps it from dividing by zero, so it behaves like a floor of one.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn min_requests_zero_trips_on_first_failure() {
        let _trace = linkerd_tracing::test::trace_init();
        let (params, gate_tx, rsps) = gate::Params::channel(1);

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            max_failures: 100, // Consecutive policy out of the way.
            threshold: 0.8,
            min_requests: 0,
            ..default_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());
        assert!(params.gate.is_open());

        // The first 5xx leaves a 0 of 1 ratio, below 0.8, and the zero floor does
        // not hold it back.
        send_err(&params, http::StatusCode::BAD_GATEWAY);
        time::advance(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());

        assert!(
            params.gate.is_shut(),
            "min_requests=0 trips on the first failing in-window sample",
        );
    }

    // An over-ceiling hint is clamped to the backoff maximum. A Duration::MAX
    // hint cannot push recovery past the 100s backoff ceiling, so the gate
    // reaches probation at the ceiling rather than sleeping forever.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn duration_max_hint_is_clamped() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            ..default_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // Trip via three consecutive 5xx.
        for _ in 0..3 {
            send_err(&params, http::StatusCode::BAD_GATEWAY);
            time::advance(time::Duration::from_millis(100)).await;
            assert_pending!(task.poll());
        }
        assert!(params.gate.is_shut(), "should trip after 3 consecutive 5xx");

        // After the 100s backoff ceiling the breaker should enter probation.
        time::sleep(time::Duration::from_secs(100)).await;
        assert_pending!(task.poll());

        assert!(
            params.gate.is_limited(),
            "a Duration::MAX hint is clamped to the backoff ceiling; the gate \
             reaches probation rather than sleeping forever"
        );
    }

    // Each trip builds a fresh backoff stream, so escalation does not survive
    // across a trip and its later recovery. After recovery the next trip starts
    // at the base backoff rather than an escalated value.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn backoff_resets_to_base_on_retrip() {
        let _trace = linkerd_tracing::test::trace_init();

        let (mut params, gate_tx, rsps) = gate::Params::channel(1);

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            max_failures: 2,
            ..default_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());
        assert!(params.gate.is_open());

        // Trip #1.
        for _ in 0..2 {
            send_err(&params, http::StatusCode::BAD_GATEWAY);
            time::advance(time::Duration::from_millis(100)).await;
            assert_pending!(task.poll());
        }
        assert!(params.gate.is_shut(), "should trip after 2 consecutive 5xx");

        // Wait out the first backoff (base 1s) and enter probation.
        time::sleep(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());

        match params.gate.state() {
            gate::State::Limited(sem) => {
                assert_eq!(sem.available_permits(), 1);
                params.gate.opened_for_test().await.unwrap().forget();
            }
            _ => panic!("Expected Limited state after first backoff"),
        }

        // The probe succeeds and the breaker reopens.
        send_ok(&params, http::StatusCode::OK);
        time::advance(time::Duration::from_millis(100)).await;
        assert_pending!(task.poll());
        assert!(
            params.gate.is_open(),
            "should reopen after a successful probe"
        );

        // Trip #2.
        for _ in 0..2 {
            send_err(&params, http::StatusCode::BAD_GATEWAY);
            time::advance(time::Duration::from_millis(100)).await;
            assert_pending!(task.poll());
        }
        assert!(
            params.gate.is_shut(),
            "should trip again after 2 consecutive 5xx"
        );

        // Advance 1s plus a small buffer. Had the backoff escalated to 2s, the
        // gate would still be shut. With a fresh stream the base is 1s.
        time::sleep(time::Duration::from_millis(1050)).await;
        assert_pending!(task.poll());

        assert!(
            params.gate.is_limited(),
            "the second trip should use the base backoff, not an escalated one; \
             closed() builds a fresh backoff stream each time"
        );
    }

    // With both policies disabled (max_failures=0 turns off consecutive
    // failures, threshold=0.0 disables the success-rate dimension), no
    // combination of errors can trip the circuit.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn both_policies_disabled_never_trips() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            max_failures: 0,
            threshold: 0.0,
            ..default_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        for _ in 0..10 {
            send_err(&params, http::StatusCode::BAD_GATEWAY);
            time::advance(time::Duration::from_secs(1)).await;
            assert_pending!(task.poll());
            assert!(
                params.gate.is_open(),
                "must stay open when both policies are disabled (5xx)"
            );
        }

        for _ in 0..10 {
            send_ok(&params, http::StatusCode::TOO_MANY_REQUESTS);
            time::advance(time::Duration::from_secs(1)).await;
            assert_pending!(task.poll());
            assert!(
                params.gate.is_open(),
                "must stay open when both policies are disabled (429)"
            );
        }
    }

    // After a long idle period (many multiples of the window), the window's own
    // expiry clears the old samples, so first-start protection re-engages and one
    // post-idle sample cannot trip the circuit.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn idle_reengages_cold_start_with_success() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            max_failures: 100,
            threshold: 0.5,
            min_requests: 3,
            window: time::Duration::from_secs(10),
            ..default_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());
        time::advance(time::Duration::from_millis(1)).await;

        for _ in 0..3 {
            send_ok(&params, http::StatusCode::OK);
            time::advance(time::Duration::from_secs(1)).await;
            assert_pending!(task.poll());
        }

        send_err(&params, http::StatusCode::BAD_GATEWAY);
        time::advance(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());
        assert!(
            params.gate.is_open(),
            "one failure among successes should not trip (ratio still above 0.5)"
        );

        // Long idle: 200s, 20x the window, far past full expiry.
        time::advance(time::Duration::from_secs(200)).await;

        // The window expired during the idle gap, so it starts empty. A post-idle
        // success leaves one in-window sample, below min_requests=3.
        send_ok(&params, http::StatusCode::OK);
        time::advance(time::Duration::from_millis(10)).await;
        assert_pending!(task.poll());
        assert!(
            params.gate.is_open(),
            "post-idle success: cold-start active (1 < min_requests)"
        );

        send_err(&params, http::StatusCode::BAD_GATEWAY);
        time::advance(time::Duration::from_millis(10)).await;
        assert_pending!(task.poll());
        assert!(
            params.gate.is_open(),
            "post-idle failure: cold-start still active (2 < min_requests)"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn idle_reengages_cold_start_with_failure() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            max_failures: 100,
            threshold: 0.5,
            min_requests: 3,
            window: time::Duration::from_secs(10),
            ..default_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());
        time::advance(time::Duration::from_millis(1)).await;

        for _ in 0..3 {
            send_ok(&params, http::StatusCode::OK);
            time::advance(time::Duration::from_secs(1)).await;
            assert_pending!(task.poll());
        }
        send_err(&params, http::StatusCode::BAD_GATEWAY);
        time::advance(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());
        assert!(
            params.gate.is_open(),
            "one failure among successes should not trip (ratio still above 0.5)"
        );

        // Long idle: 200s, 20x the window.
        time::advance(time::Duration::from_secs(200)).await;

        // The window expired during the idle gap. Failures after idle cannot trip
        // until min_requests fresh samples accumulate.
        send_err(&params, http::StatusCode::BAD_GATEWAY);
        time::advance(time::Duration::from_millis(10)).await;
        assert_pending!(task.poll());
        assert!(
            params.gate.is_open(),
            "post-idle failure 1/3: cold-start active (1 < min_requests)"
        );

        send_err(&params, http::StatusCode::BAD_GATEWAY);
        time::advance(time::Duration::from_millis(10)).await;
        assert_pending!(task.poll());
        assert!(
            params.gate.is_open(),
            "post-idle failure 2/3: cold-start active (2 < min_requests)"
        );

        // The third failure reaches min_requests=3 with a 0/3 in-window ratio.
        send_err(&params, http::StatusCode::BAD_GATEWAY);
        time::advance(time::Duration::from_millis(10)).await;
        assert_pending!(task.poll());
        assert!(
            params.gate.is_shut(),
            "post-idle failure 3/3: cold-start satisfied, ratio 0.0 < threshold 0.5"
        );
    }

    // A closed response channel must end the breaker task even while it is held in
    // the backoff wait. recv() on a closed channel yields None right away, so the
    // drain step has to treat None as teardown rather than loop on it. The gate
    // receiver is kept alive here, so gate.lost() never fires and the task ends only
    // through the closed-channel check.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn wait_for_backoff_terminates_when_channel_closes() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);

        // max_failures=1 trips the breaker on the first 5xx, dropping it into the
        // closed state and the backoff wait.
        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            max_failures: 1,
            ..default_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());
        assert!(params.gate.is_open());

        send_err(&params, http::StatusCode::INTERNAL_SERVER_ERROR);
        assert_pending!(task.poll());
        assert!(
            params.gate.is_shut(),
            "a single 5xx should trip the breaker"
        );

        // Close the channel while the breaker waits out the backoff. The task
        // must finish rather than spin on None.
        drop(params.responses);
        assert!(
            task.poll().is_ready(),
            "breaker must terminate when the response channel closes during backoff"
        );
    }

    // Same teardown as above, but a second gate receiver outlives the endpoint
    // service so gate.lost() can never resolve. Without the closed-channel check the
    // backoff wait would spin forever on None, and with it the task exits. A real
    // task is needed so the Gate plus BroadcastClassification stack can drive a
    // classification through the channel.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn wait_for_backoff_terminates_with_leaked_gate() {
        use linkerd_app_core::{
            proxy::http::{classify::BroadcastClassification, BoxBody},
            svc::{self, ServiceExt},
            Error,
        };
        use tokio::task::yield_now;

        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);
        // Model a stack that retains gate state after the endpoint service is
        // dropped by holding a second gate receiver.
        let leaked_gate = params.gate.clone();

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            max_failures: 1,
            ..default_config(gate_tx, rsps)
        });
        let breaker = tokio::spawn(breaker.run());

        let svc = svc::Gate::new(
            params.gate,
            BroadcastClassification::<classify::Response, _>::new(
                params.responses,
                svc::mk(|_: http::Request<BoxBody>| async move {
                    Ok::<_, Error>(
                        http::Response::builder()
                            .status(http::StatusCode::INTERNAL_SERVER_ERROR)
                            .body(BoxBody::default())
                            .unwrap(),
                    )
                }),
            ),
        );

        let req = http::Request::builder()
            .extension(classify::Response::default())
            .body(BoxBody::default())
            .unwrap();
        let rsp = svc.oneshot(req).await.expect("service must succeed");
        // Dropping the response emits a final classification, then drops the last
        // sender so the channel closes.
        drop(rsp);

        for _ in 0..16 {
            if leaked_gate.is_shut() {
                break;
            }
            yield_now().await;
        }
        assert!(
            leaked_gate.is_shut(),
            "breaker should enter the closed state"
        );

        for _ in 0..16 {
            if breaker.is_finished() {
                break;
            }
            yield_now().await;
        }
        assert!(
            breaker.is_finished(),
            "breaker must terminate when the response channel closes, \
             even while a second gate receiver survives"
        );
        breaker.await.expect("breaker task must not panic");
    }

    // Probation limits the gate to a single in-flight probe. The breaker calls
    // gate.limit(1), so the semaphore offers exactly one permit. Acquiring it
    // models the one admitted probe, and a second concurrent request finds no permit.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn probation_admits_single_probe() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            ..default_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // Trip via three consecutive 5xx.
        for _ in 0..3 {
            send_err(&params, http::StatusCode::BAD_GATEWAY);
            time::advance(time::Duration::from_millis(100)).await;
            assert_pending!(task.poll());
        }
        assert!(params.gate.is_shut());

        // Wait out the base backoff so the breaker enters probation.
        time::sleep(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());

        // The gate exposes a one-permit semaphore. Draining that permit leaves
        // none for a would-be second probe, which is the single-probe bound.
        match params.gate.state() {
            gate::State::Limited(sem) => {
                assert_eq!(sem.available_permits(), 1, "probation admits one probe");
                let permit = sem.try_acquire().expect("the single probe acquires");
                assert_eq!(
                    sem.available_permits(),
                    0,
                    "a second concurrent probe finds no permit",
                );
                assert!(
                    sem.try_acquire().is_err(),
                    "the semaphore admits only one probe at a time",
                );
                drop(permit);
            }
            other => panic!("expected Limited state, got {other:?}"),
        }
    }

    // After a success-rate trip, an optimistic reopen with steady healthy traffic
    // keeps the windowed ratio above the threshold. A single failure right after
    // recovery falls under the first-start sample floor, so the circuit stays open
    // rather than trip again on one old failure.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn success_rate_recovers_after_decay() {
        let _trace = linkerd_tracing::test::trace_init();

        let (mut params, gate_tx, rsps) = gate::Params::channel(1);

        // Consecutive failures are off so only success rate drives the trip and the
        // later recovery. A three-sample floor means one failure after recovery is
        // not enough evidence to trip again, and decay=10s keeps the arithmetic
        // readable.
        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            max_failures: 0,
            threshold: 0.5,
            min_requests: 3,
            window: time::Duration::from_secs(10),
            ..default_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // Three successes satisfy the first-start floor with a healthy ratio.
        for _ in 0..3 {
            send_ok(&params, http::StatusCode::OK);
            time::advance(time::Duration::from_secs(1)).await;
            assert_pending!(task.poll());
        }

        // 429s drag the windowed ratio under 0.5 and trip. Stop at the trip so the
        // backoff timer is not advanced while more failures arrive.
        for _ in 0..8 {
            send_ok(&params, http::StatusCode::TOO_MANY_REQUESTS);
            time::advance(time::Duration::from_secs(1)).await;
            assert_pending!(task.poll());
            if params.gate.is_shut() {
                break;
            }
        }
        assert!(params.gate.is_shut(), "low success rate should trip");

        // Wait out the base backoff and let the probe succeed.
        time::sleep(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());
        match params.gate.state() {
            gate::State::Limited(_) => {
                params.gate.opened_for_test().await.unwrap().forget();
            }
            other => panic!("expected Limited state, got {other:?}"),
        }
        send_ok(&params, http::StatusCode::OK);
        time::advance(time::Duration::from_millis(10)).await;
        assert_pending!(task.poll());
        assert!(
            params.gate.is_open(),
            "a successful probe reopens the circuit"
        );

        // Recovery resets the window. One failure leaves a single in-window
        // sample, below the three-sample floor, so the rate dimension cannot act
        // on it yet.
        send_err(&params, http::StatusCode::BAD_GATEWAY);
        time::advance(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());
        assert!(
            params.gate.is_open(),
            "a single failure after recovery should not re-trip",
        );

        // Steady successes raise the ratio well above 0.5, and the lone failure
        // eventually ages out of the window. The circuit holds open throughout.
        for _ in 0..20 {
            send_ok(&params, http::StatusCode::OK);
            time::advance(time::Duration::from_secs(1)).await;
            assert_pending!(task.poll());
            assert!(
                params.gate.is_open(),
                "a recovered success rate should keep the circuit open",
            );
        }
    }

    // With both policies enabled, a stream of 429s lowers the success rate without
    // raising the consecutive counter, since 429 is not a hard failure. The
    // circuit trips on low success rate while consecutive failures stay at zero, and
    // the charged reason reflects that.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn dual_mode_success_rate_trips_before_consecutive() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);
        let (trip_tx, mut trip_rx) = mpsc::unbounded_channel();

        // Both mechanisms active: a five-failure consecutive budget and an 80%
        // success-rate floor with a three-request first start.
        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            max_failures: 5,
            threshold: 0.8,
            min_requests: 3,
            window: time::Duration::from_secs(10),
            ..default_config(gate_tx, rsps)
        })
        .with_trip_observer(trip_tx);
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // Seed one success so the window holds a healthy sample and the first-start
        // sample count begins climbing.
        send_ok(&params, http::StatusCode::OK);
        time::advance(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());

        // 429s push the windowed ratio under 0.8 while consecutive failures never
        // move, since the consecutive policy treats 429 as success. After one
        // success and two 429s the window reads 1 of 3, under 0.8 with min_requests
        // met, so the trip happens there. Break at that response to check while the
        // gate is shut. A further 429 would advance time past the base backoff and
        // reopen the gate for a probe, which would hide the shut state.
        let mut shut = false;
        for _ in 0..4 {
            send_ok(&params, http::StatusCode::TOO_MANY_REQUESTS);
            time::advance(time::Duration::from_secs(1)).await;
            assert_pending!(task.poll());
            if params.gate.is_shut() {
                shut = true;
                break;
            }
        }

        assert!(
            shut,
            "success rate should trip while consecutive failures stay at zero",
        );
        // The reason proves the consecutive policy did not trip. The trip check
        // examines consecutive failures first, so a LowSuccessRate verdict means
        // the consecutive counter never reached max_failures.
        assert_eq!(
            trip_rx.try_recv(),
            Ok(TripReason::LowSuccessRate),
            "the trip is attributed to low success rate, not consecutive failures",
        );
    }

    // A consecutive-failure trip in a single-policy configuration reports
    // ConsecutiveFailures. Success rate is disabled (threshold 0.0), so the only
    // way to trip is the consecutive counter reaching max_failures.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn consecutive_trip_reports_consecutive_reason() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);
        let (trip_tx, mut trip_rx) = mpsc::unbounded_channel();

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            max_failures: 3,
            threshold: 0.0,
            ..default_config(gate_tx, rsps)
        })
        .with_trip_observer(trip_tx);
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        for _ in 0..3 {
            send_err(&params, http::StatusCode::BAD_GATEWAY);
            time::advance(time::Duration::from_millis(100)).await;
            assert_pending!(task.poll());
        }

        assert!(params.gate.is_shut(), "three 5xx trip consecutive failures");
        assert_eq!(
            trip_rx.try_recv(),
            Ok(TripReason::ConsecutiveFailures),
            "a consecutive-failure trip reports ConsecutiveFailures",
        );
    }

    // A success-rate trip in a single-policy configuration reports LowSuccessRate.
    // Consecutive failures are disabled (max_failures 0), so only a low windowed
    // ratio can trip.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn success_rate_trip_reports_low_success_rate_reason() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);
        let (trip_tx, mut trip_rx) = mpsc::unbounded_channel();

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            max_failures: 0,
            threshold: 0.5,
            min_requests: 1,
            window: time::Duration::from_secs(10),
            ..default_config(gate_tx, rsps)
        })
        .with_trip_observer(trip_tx);
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        send_ok(&params, http::StatusCode::OK);
        time::advance(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());

        for _ in 0..8 {
            send_ok(&params, http::StatusCode::TOO_MANY_REQUESTS);
            time::advance(time::Duration::from_secs(1)).await;
            assert_pending!(task.poll());
            if params.gate.is_shut() {
                break;
            }
        }

        assert!(params.gate.is_shut(), "low success rate trips the circuit");
        assert_eq!(
            trip_rx.try_recv(),
            Ok(TripReason::LowSuccessRate),
            "a success-rate trip reports LowSuccessRate",
        );
    }

    // When one response crosses both thresholds at once, consecutive failures win
    // the charge, since the trip check examines them first. The config is tuned
    // so the third 5xx reaches max_failures and, with min_requests also at three,
    // drops the windowed ratio below the threshold on that same response.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn simultaneous_threshold_cross_reports_consecutive_reason() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);
        let (trip_tx, mut trip_rx) = mpsc::unbounded_channel();

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            max_failures: 3,
            threshold: 0.8,
            min_requests: 3,
            window: time::Duration::from_secs(10),
            ..default_config(gate_tx, rsps)
        })
        .with_trip_observer(trip_tx);
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // Three 5xx leave a 0 of 3 ratio, below 0.8, exactly as the consecutive
        // counter reaches 3 and min_requests is met. Both conditions hold on the
        // third response.
        for _ in 0..3 {
            send_err(&params, http::StatusCode::BAD_GATEWAY);
            time::advance(time::Duration::from_secs(1)).await;
            assert_pending!(task.poll());
        }

        assert!(params.gate.is_shut(), "the third 5xx trips the circuit");
        assert_eq!(
            trip_rx.try_recv(),
            Ok(TripReason::ConsecutiveFailures),
            "a simultaneous cross is attributed to consecutive failures",
        );
    }

    // A probe that never resolves a class must not strand the breaker. After the
    // backoff window passes with no verdict, probation shuts the gate again,
    // advances backoff, and keeps retrying, so the endpoint stays ejected and a
    // still-broken endpoint never reopens by accident.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn probe_timeout_reshuts_gate() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);
        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            ..default_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // Trip via three consecutive 5xx.
        for _ in 0..3 {
            send_err(&params, http::StatusCode::BAD_GATEWAY);
            time::advance(time::Duration::from_millis(100)).await;
            assert_pending!(task.poll());
        }
        assert!(params.gate.is_shut());

        // The base backoff (1s) elapses and the breaker enters probation. The
        // probe deadline is the backoff ceiling (100s), decoupled from this
        // window.
        time::sleep(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());
        assert!(params.gate.is_limited(), "breaker should be in probation");

        // No probe verdict arrives. The deadline is now the backoff ceiling of 100s
        // rather than the 1s window just waited, so the wait runs past 100s for the
        // probe to expire, and then the breaker shuts the gate again and begins the
        // next 2s backoff.
        time::sleep(time::Duration::from_secs(101)).await;
        assert_pending!(task.poll());
        assert!(
            params.gate.is_shut(),
            "a timed-out probe must re-shut the gate, not reopen it",
        );
        assert!(!params.gate.is_open(), "the endpoint stays ejected");

        // The escalated 2s backoff is in effect, proving the timeout looped rather
        // than exiting. At 1s into it the gate is still shut, and the base 1s window
        // would already have reached probation.
        time::sleep(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());
        assert!(
            params.gate.is_shut(),
            "the second backoff is longer than the base, so the gate stays shut at 1s",
        );
    }

    // A healthy endpoint whose probe verdict lands after the base backoff window but
    // before the ceiling still reopens. The deadline is the backoff ceiling rather
    // than the window just waited, so a slow but good probe is not cut into a silent
    // failure. Were the deadline the base 1s window, the probe slot would already
    // have expired and shut the gate before the verdict arrived.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn slow_healthy_probe_within_ceiling_reopens() {
        let _trace = linkerd_tracing::test::trace_init();

        let (mut params, gate_tx, rsps) = gate::Params::channel(1);
        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            ..default_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // Trip via three consecutive 5xx.
        for _ in 0..3 {
            send_err(&params, http::StatusCode::BAD_GATEWAY);
            time::advance(time::Duration::from_millis(100)).await;
            assert_pending!(task.poll());
        }
        assert!(params.gate.is_shut());

        // The base backoff (1s) elapses and the breaker enters probation.
        time::sleep(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());
        assert!(params.gate.is_limited(), "breaker should be in probation");

        // Route a probe: take the permit the breaker opened.
        params.gate.opened_for_test().await.unwrap().forget();

        // Advance well past the base 1s window but far short of the 100s ceiling.
        // The probe slot is still live: a slow healthy peer has not yet answered,
        // and the gate stays limited rather than re-shutting on a phantom timeout.
        time::sleep(time::Duration::from_secs(5)).await;
        assert_pending!(task.poll());
        assert!(
            params.gate.is_limited(),
            "a probe slower than the base backoff is still pending under the ceiling",
        );

        // The verdict finally lands within the ceiling. The slow but healthy
        // probe reopens the circuit.
        send_ok(&params, http::StatusCode::OK);
        assert_pending!(task.poll());
        assert!(
            params.gate.is_open(),
            "a healthy verdict within the ceiling reopens the gate",
        );
    }

    // A class buffered before the probe is admitted is leftover data from before the
    // trip. The gate was shut for the whole preceding backoff, so probation drops
    // anything queued and waits for a fresh response. With only an old success
    // present no verdict follows, so the probe times out and shuts the gate again
    // rather than reopen on the old value.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn stale_pre_trip_class_not_consumed_as_probe() {
        let _trace = linkerd_tracing::test::trace_init();

        // Capacity 2 holds both the tripping traffic and the old class.
        let (params, gate_tx, rsps) = gate::Params::channel(2);
        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            ..default_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // Trip via three consecutive 5xx.
        for _ in 0..3 {
            send_err(&params, http::StatusCode::BAD_GATEWAY);
            time::advance(time::Duration::from_millis(100)).await;
            assert_pending!(task.poll());
        }
        assert!(params.gate.is_shut());

        // Queue an old success during the shut gate, ahead of any probe
        // admission. A 200 OK would read as a successful probe if it were taken as
        // the verdict.
        send_ok(&params, http::StatusCode::OK);

        // Backoff elapses and the breaker enters probation, and here the old OK is
        // drained during the backoff wait, so this case exercises the full
        // invariant that a success from before the trip never reopens the breaker.
        //
        // A narrower race also exists where a class can land in the gap between the
        // backoff wait and the probe. The drain on probation entry closes that gap,
        // and only real request interleaving through the pool can force it reliably.
        time::sleep(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());

        // No fresh verdict is sent. If the old OK had counted as the probe the gate
        // would be open here. The probe deadline is the backoff ceiling of 100s
        // rather than the 1s window just waited, so the wait runs past 100s for the
        // probe to expire, and past the deadline the probe times out and the gate
        // shuts again.
        time::sleep(time::Duration::from_secs(101)).await;
        assert_pending!(task.poll());
        assert!(
            !params.gate.is_open(),
            "a stale pre-trip success must not reopen the breaker",
        );
        assert!(
            params.gate.is_shut(),
            "with no fresh verdict the probe times out and re-shuts",
        );
    }

    // A longer Retry-After/pushback hint sent by a later probe raises the backoff
    // floor for the waits that follow. The hint is re-read before every backoff wait,
    // so one recorded after the first probe still applies. Here a 10s hint recorded
    // before the second backoff floors it well past the 2s the exponential schedule
    // alone would give.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn cycle_2_fresh_hint_honored() {
        let _trace = linkerd_tracing::test::trace_init();

        let (mut params, gate_tx, rsps) = gate::Params::channel(1);
        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            ..default_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // Trip via three consecutive 5xx. The store is empty, so the first backoff
        // is the base 1s.
        for _ in 0..3 {
            send_err(&params, http::StatusCode::BAD_GATEWAY);
            time::advance(time::Duration::from_millis(100)).await;
            assert_pending!(task.poll());
        }
        assert!(params.gate.is_shut());

        // The first backoff (1s) elapses and probation begins.
        time::sleep(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());
        match params.gate.state() {
            gate::State::Limited(_) => {
                params.gate.opened_for_test().await.unwrap().forget();
            }
            other => panic!("expected Limited state, got {other:?}"),
        }

        send_err(&params, http::StatusCode::BAD_GATEWAY);
        time::advance(time::Duration::from_millis(10)).await;
        assert_pending!(task.poll());
        assert!(params.gate.is_shut(), "a failed probe re-shuts the gate");

        // The plain second backoff would be 2s. At 5s the gate is still shut, since
        // the 10s hint raised the floor for this iteration.
        time::sleep(time::Duration::from_secs(5)).await;
        assert_pending!(task.poll());
        assert!(
            params.gate.is_shut(),
            "the fresh 10s hint floors the second backoff past the 2s base",
        );

        // Past the hint window the gate finally enters probation.
        time::sleep(time::Duration::from_secs(6)).await;
        assert_pending!(task.poll());
        assert!(
            params.gate.is_limited(),
            "once the 10s hint elapses the breaker probes again",
        );
    }
}
