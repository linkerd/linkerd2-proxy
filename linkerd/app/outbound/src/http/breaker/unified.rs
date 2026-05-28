//! Unified circuit breaker combining consecutive failures and success rate policies.
//!
//! This module implements a single circuit breaker that implements two policies:
//! - Consecutive failures: Trips after N consecutive 5xx responses
//! - Success rate (EWMA): Trips when success rate drops below threshold (treats 429 as failure)
//!
//! ## Trip Conditions
//!
//! Either condition can trip the circuit:
//!
//! * Consecutive failures >= `max_failures` (0 = disabled, no cold-start protection)
//! * EWMA success rate < `threshold` AND request_count >= `min_requests` (cold-start protected)
//!
//! Consecutive failures are checked before success rate, so a response that crosses
//! both thresholds at once is charged to [`TripReason::ConsecutiveFailures`]. The
//! reason is for observability only. The circuit trips on either condition either way.
//!
//! ## Cold-Start Protection
//!
//! The EWMA success rate policy has three layers of cold-start protection:
//!
//! 1. Optimistic initialization: EWMA starts at 1.0 (100% success). This prevents
//!    immediate tripping on first failures.
//!
//! 2. Minimum request threshold: The circuit cannot trip on low success rate until
//!    `min_requests` have been observed. This ensures statistical significance.
//!
//! 3. Idle restart: After a long idle period (3x the decay window), the request
//!    counter resets, re-requiring `min_requests` new observations before
//!    the success rate policy can trip. This prevents a single post-idle response
//!    from dominating the EWMA and tripping the circuit.
//!
//! Cold-start protection does NOT apply to consecutive failures policy. If an endpoint
//! returns `max_failures` consecutive 5xx errors, it will trip immediately. This is
//! intentional because repeated failures are a strong signal regardless of sample size.
//!
//! ## Recovery Conditions
//!
//! During probation, the probe success check is mode-aware:
//!
//! - In dual mode (success rate active), a probe must be non-5xx AND non-429 for
//!   HTTP, or Ok for gRPC, to succeed. A 429 during probation is treated as failure
//!   because it indicates the endpoint is still rate limiting.
//!
//! - In consecutive-only mode (success rate disabled), the probe delegates to
//!   `class.is_success()` to preserve the behavior of the consecutive-failures
//!   breaker it replaces. This ensures 429 responses are evaluated by the default
//!   classifier rather than being unconditionally treated as failure.
//!
//! ## Retry-After Integration
//!
//! When a 429 response includes a `Retry-After` header, the duration is stored in
//! a [`RetryAfterStore`] and used as a floor for the first backoff after trip.
//!
//! Similarly, when a gRPC RESOURCE_EXHAUSTED response includes a `grpc-retry-pushback-ms`
//! header or trailer, the duration is stored in a [`GrpcRetryPushbackStore`] and used
//! as a floor for the first backoff. If both HTTP and gRPC hints exist, the maximum
//! is used.
//!
//! ## gRPC RESOURCE_EXHAUSTED Handling
//!
//! gRPC's RESOURCE_EXHAUSTED (code 8) is treated equivalently to HTTP 429:
//! - Counts as failure for success rate EWMA
//! - Won't count for consecutive failures (not a hard failure)
//! - During probation only gRPC Ok (code 0) is considered success

use super::{
    retry_after::{GrpcRetryPushbackStore, RetryAfterStore},
    TripReason,
};
use futures::stream::StreamExt;
use linkerd_app_core::proxy::http::classify::gate;
use linkerd_app_core::{classify, exp_backoff, exp_backoff::ExponentialBackoff};
use linkerd_ewma::{Ewma, MIN_DECAY};
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};
use tonic::Code;

/// Idle multiplier for cold-start restart. When the gap since the last response
/// exceeds this many decay windows, the request counter resets so that
/// `min_requests` new samples are required before the success rate policy can
/// trip. At 3x, alpha ~= 0.95: a single sample after the idle gap would account
/// for ~95% of the EWMA, so one error after a quiet period could trip the
/// circuit on its own. Re-arming cold-start protection prevents that.
///
// TODO: expose this as configuration once the mechanism has seen real traffic.
const IDLE_COLD_START_FACTOR: u32 = 3;

/// Unified circuit breaker combining consecutive failures and success rate policies.
///
/// This policy tracks both consecutive failures (for 5xx responses) and EWMA success
/// rate (treating both 5xx and 429 as failures). Either condition can trip the circuit.
/// Cold-start protection only applies to success rate, not consecutive failures.
pub struct UnifiedBreaker {
    // === Configuration ===
    /// Maximum consecutive failures (5xx only) before trip.
    max_failures: usize,
    /// Success rate threshold (0.0-1.0) - trip when EWMA drops below this.
    threshold: f64,
    /// EWMA decay window for success rate tracking.
    decay: Duration,
    /// Exponential backoff configuration for recovery.
    backoff: ExponentialBackoff,
    /// Minimum requests before circuit can trip (cold-start protection).
    min_requests: usize,

    // === Gate Control ===
    /// Gate channel to control endpoint readiness (direct - no coordination wrapper).
    gate: gate::Tx,
    /// Channel receiving response classifications.
    rsps: mpsc::Receiver<classify::Class>,

    // === Retry-After Integration ===
    /// Shared store for HTTP Retry-After hints from 429 responses.
    retry_after_store: RetryAfterStore,
    /// Shared store for gRPC retry pushback hints from RESOURCE_EXHAUSTED responses.
    grpc_retry_pushback_store: GrpcRetryPushbackStore,
    /// Maximum Retry-After duration the proxy will honor (clamping cap).
    max_duration: Duration,

    /// Lets tests see the reason charged to each trip without affecting behavior.
    #[cfg(test)]
    trip_observer: Option<mpsc::UnboundedSender<TripReason>>,
}

/// Internal state tracked during the open phase.
struct OpenState {
    /// Current consecutive failure count (5xx only).
    consecutive_failures: usize,
    /// EWMA tracker for success rate (5xx and 429 count as failure).
    ewma: Ewma,
    /// Requests since last cold-start reset (for cold-start protection).
    /// Resets on recovery and after idle periods exceeding the cold-start
    /// threshold.
    request_count: usize,
    /// Timestamp of the most recent response, used to detect idle gaps.
    /// Tracked apart from the EWMA. The average's timestamp freezes while the
    /// circuit is tripped and probing, so reading idle from it would miss quiet
    /// periods that span a trip-recover cycle.
    last_seen: Instant,
}

impl OpenState {
    /// Create new open state with optimistic EWMA initialization.
    ///
    /// The EWMA starts at 1.0 (100% success rate) rather than 0.0 to implement
    /// "optimistic cold-start": new or recovered endpoints are assumed healthy
    /// until proven otherwise. Combined with `min_requests` cold-start protection,
    /// this prevents:
    /// - Immediate tripping on first failure (EWMA would drop below threshold)
    /// - False positives during low-traffic periods where single failures have
    ///   outsized impact on the EWMA
    ///
    /// The EWMA will naturally decay toward actual observed success rate as
    /// requests are processed. After `min_requests` observations, the EWMA
    /// represents a statistically meaningful estimate.
    fn new(decay: Duration, now: Instant) -> Self {
        if decay < MIN_DECAY {
            tracing::warn!(
                ?decay,
                min = ?MIN_DECAY,
                "success-rate decay below minimum, will be clamped by EWMA constructor"
            );
        }
        Self {
            consecutive_failures: 0,
            ewma: Ewma::new_with_value(decay, now, 1.0),
            request_count: 0,
            last_seen: now,
        }
    }

    /// Return the state to its initial values after recovery.
    fn reset(&mut self, decay: Duration, now: Instant) {
        *self = Self::new(decay, now);
    }
}

/// Configuration for [`UnifiedBreaker::new`].
#[derive(Debug)]
pub(crate) struct UnifiedBreakerConfig {
    pub(crate) max_failures: usize,
    pub(crate) threshold: f64,
    pub(crate) decay: Duration,
    pub(crate) backoff: ExponentialBackoff,
    pub(crate) min_requests: usize,
    pub(crate) gate: gate::Tx,
    pub(crate) rsps: mpsc::Receiver<classify::Class>,
    pub(crate) retry_after_store: RetryAfterStore,
    pub(crate) grpc_retry_pushback_store: GrpcRetryPushbackStore,
    pub(crate) max_duration: Duration,
}

impl UnifiedBreaker {
    /// Create a new unified circuit breaker.
    pub fn new(config: UnifiedBreakerConfig) -> Self {
        let UnifiedBreakerConfig {
            max_failures,
            threshold,
            decay,
            backoff,
            min_requests,
            gate,
            rsps,
            retry_after_store,
            grpc_retry_pushback_store,
            max_duration,
        } = config;
        Self {
            max_failures,
            threshold,
            decay,
            backoff,
            min_requests,
            gate,
            rsps,
            retry_after_store,
            grpc_retry_pushback_store,
            max_duration,
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

    /// Run the circuit breaker state machine.
    ///
    /// The state machine cycles through:
    /// - Open: accept requests, track failures until trip condition met
    /// - Closed: reject requests, wait for backoff
    /// - Probation: allow single probe request
    /// - If probe succeeds -> Open, if probe fails -> Closed
    pub async fn run(mut self) {
        let mut state = OpenState::new(self.decay, Instant::now());

        loop {
            // Open state: track failures until trip condition
            let trip_reason = match self.open(&mut state).await {
                Ok(reason) => reason,
                Err(()) => return, // Gate lost
            };

            tracing::info!(?trip_reason, "Unified circuit breaker tripped");

            #[cfg(test)]
            if let Some(tx) = self.trip_observer.as_ref() {
                let _ = tx.send(trip_reason);
            }

            // Closed state: wait for backoff, then probe
            match self.closed(trip_reason).await {
                Ok(()) => {
                    // Probe succeeded, reset state for reopening
                    state.reset(self.decay, Instant::now());
                    tracing::info!("Unified circuit breaker reopened");
                }
                Err(()) => return, // Gate lost
            }
        }
    }

    /// Open state: track both policies until either trips.
    ///
    /// Returns the reason for trip, or `Err(())` if gate is lost.
    async fn open(&mut self, state: &mut OpenState) -> Result<TripReason, ()> {
        tracing::debug!("Open");

        self.gate.open().map_err(|_| ())?;

        // Saturate rather than multiply directly. A very large decay set by an
        // operator could push `decay * IDLE_COLD_START_FACTOR` past
        // `Duration::MAX` and panic. Computing it once also keeps the multiply
        // out of the loop.
        let idle_reset = self.decay.saturating_mul(IDLE_COLD_START_FACTOR);

        loop {
            let class = tokio::select! {
                rsp = self.rsps.recv() => rsp.ok_or(())?,
                _ = self.gate.lost() => return Err(()),
            };

            let now = Instant::now();

            // Reset cold-start protection after idle periods where the
            // EWMA has decayed enough that one sample would dominate. Idle is
            // measured against the last response rather than the EWMA, whose
            // timestamp stops across a trip-recover cycle.
            if now.saturating_duration_since(state.last_seen) > idle_reset {
                tracing::debug!(
                    previous_count = state.request_count,
                    "Re-engaging cold-start protection after idle"
                );
                state.request_count = 0;
            }

            state.request_count = state.request_count.saturating_add(1);
            state.last_seen = now;

            let is_configured_failure = !class.is_success();
            let degrades_success_rate = match &class {
                classify::Class::Http(Ok(status)) => *status == http::StatusCode::TOO_MANY_REQUESTS,
                classify::Class::Http(Err(_)) => true,
                classify::Class::Grpc(Ok(code)) => *code == Code::ResourceExhausted,
                classify::Class::Grpc(Err(_)) => true,
                classify::Class::Error(_) => true,
            };

            // Update consecutive configured failures (skip when policy disabled)
            if self.max_failures > 0 {
                if is_configured_failure {
                    state.consecutive_failures = state.consecutive_failures.saturating_add(1);
                } else {
                    state.consecutive_failures = 0;
                }
            }

            // Update EWMA (configured failures, typically 5xx, and 429 are failures).
            // `Ewma::add` drops samples whose timestamp matches the stored one, so
            // responses that share a single instant combine into the first. The request
            // counter still increments per response and can lead the EWMA by one.
            let value = if degrades_success_rate { 0.0 } else { 1.0 };
            state.ewma.add(value, now);
            let rate = state.ewma.get();

            tracing::trace!(
                ?class,
                consecutive_failures = state.consecutive_failures,
                success_rate = %rate,
                request_count = state.request_count,
                min_requests = self.min_requests,
                "Response"
            );

            // Either condition can trip. Consecutive failures has no cold-start, success
            // rate does. The reason reflects whichever condition `should_trip` checks first.
            if let Some(trip_reason) = self.should_trip(state) {
                return Ok(trip_reason);
            }
        }
    }

    /// Check if the circuit should trip based on current state.
    ///
    /// Returns the trip reason if a trip condition is met, `None` otherwise.
    ///
    /// Note that cold-start protection only applies to success rate, not consecutive failures.
    ///
    /// Consecutive failures come first. When a single response crosses both the
    /// consecutive-failure and success-rate thresholds, the reported reason is
    /// [`TripReason::ConsecutiveFailures`] because that condition is checked first. The
    /// difference is for observability only, since either condition trips the circuit.
    fn should_trip(&self, state: &OpenState) -> Option<TripReason> {
        // max_failures=0 disables the consecutive failure policy entirely.
        if self.max_failures == 0 {
            // Skip consecutive failures check. Only success rate can trip.
        } else if state.consecutive_failures >= self.max_failures {
            // Condition 1: Consecutive failures threshold exceeded (no cold-start protection)
            // This can trip immediately after max_failures consecutive 5xx errors.
            return Some(TripReason::ConsecutiveFailures);
        }

        // Condition 2: Success rate dropped below threshold (with cold-start protection)
        // Requires min_requests for accurate EWMA estimation before tripping.
        if state.request_count >= self.min_requests && state.ewma.get() < self.threshold {
            return Some(TripReason::LowSuccessRate);
        }

        None
    }

    /// Closed state: backoff with optional Retry-After extension.
    ///
    /// Waits for backoff to expire, then enters probation. If the probe
    /// succeeds, returns `Ok(())`. If it fails, resumes the backoff loop.
    async fn closed(&mut self, trip_reason: TripReason) -> Result<(), ()> {
        let mut backoff = self.backoff.stream();
        let mut first_iteration = true;

        loop {
            // Shut the gate
            tracing::debug!(backoff = ?backoff.duration(), ?trip_reason, "Shut");
            self.gate.shut().map_err(|_| ())?;

            // Wait for backoff, optionally extended by Retry-After/pushback hint on first iteration
            let retry_after_hint = if first_iteration {
                first_iteration = false;
                self.take_combined_hint()
            } else {
                None
            };

            self.wait_for_backoff(&mut backoff, retry_after_hint)
                .await?;

            // Enter probation and wait for probe response
            let class = self.probation().await?;
            tracing::trace!(?class, "Probe response");

            // Check probe result (mode-aware: consecutive-only preserves old semantics)
            if self.is_probe_success(&class) {
                return Ok(());
            }
            // Probe failed, continue backoff loop
        }
    }

    /// Take combined HTTP Retry-After and gRPC pushback hints, returning the maximum.
    ///
    /// A hint is honored as long as it is no older than the configured maximum
    /// hint duration. The store already subtracts elapsed time, so the remaining
    /// wait reflects how much of the hint is still in the future. If both HTTP
    /// and gRPC hints exist, the maximum duration is returned.
    fn take_combined_hint(&self) -> Option<Duration> {
        let http_hint = self.retry_after_store.take(self.max_duration);
        let grpc_hint = self.grpc_retry_pushback_store.take(self.max_duration);

        // Cap each hint individually so per-source tracing shows which one was clamped.
        let http_hint = http_hint.map(|h| {
            if h > self.max_duration {
                tracing::warn!(
                    hint = ?h,
                    max = ?self.max_duration,
                    "HTTP Retry-After hint exceeds maximum, clamping"
                );
                self.max_duration
            } else {
                h
            }
        });
        let grpc_hint = grpc_hint.map(|g| {
            if g > self.max_duration {
                tracing::warn!(
                    hint = ?g,
                    max = ?self.max_duration,
                    "gRPC pushback hint exceeds maximum, clamping"
                );
                self.max_duration
            } else {
                g
            }
        });

        match (http_hint, grpc_hint) {
            (Some(h), Some(g)) => {
                let combined = h.max(g);
                tracing::debug!(
                    http_hint = ?h,
                    grpc_hint = ?g,
                    combined = ?combined,
                    "Combined HTTP and gRPC hints (max-value-wins)"
                );
                Some(combined)
            }
            (Some(h), None) => {
                tracing::debug!(hint = ?h, "Using HTTP Retry-After hint");
                Some(h)
            }
            (None, Some(g)) => {
                tracing::debug!(hint = ?g, "Using gRPC pushback hint");
                Some(g)
            }
            (None, None) => None,
        }
    }

    /// Check if a probe response indicates successful recovery.
    ///
    /// This check is mode-aware to preserve backwards compatibility:
    ///
    /// In consecutive-only mode (`min_requests == usize::MAX`), the probe
    /// delegates to `class.is_success()`. This matches the behavior of the
    /// previous consecutive-failures breaker, where 429 responses were
    /// evaluated by the default classifier rather than being unconditionally
    /// treated as failure.
    ///
    /// In dual mode (success rate active), the check is stricter: HTTP probes
    /// must be non-5xx AND non-429, and gRPC probes must be Ok. This prevents
    /// reopening the circuit when the endpoint is still rate limiting.
    fn is_probe_success(&self, class: &classify::Class) -> bool {
        if self.min_requests == usize::MAX {
            // Consecutive-only mode: preserve old is_success() semantics.
            // The existing consecutive-failures breaker used class.is_success()
            // which lets the default classifier decide whether 429 is success
            // or failure. We must not change this behavior.
            class.is_success()
        } else {
            // Dual mode (success_rate active): stricter check.
            // 429 and RESOURCE_EXHAUSTED count as probe failure because they
            // indicate the endpoint is rate limiting, which the success-rate
            // EWMA should track.
            match class {
                classify::Class::Http(Ok(status)) => {
                    !status.is_server_error() && *status != http::StatusCode::TOO_MANY_REQUESTS
                }
                classify::Class::Http(Err(_)) => false, // 5xx failure
                classify::Class::Grpc(Ok(code)) => *code == Code::Ok,
                classify::Class::Grpc(Err(_)) => false,
                _ => class.is_success(),
            }
        }
    }

    /// Wait for the backoff duration to expire, optionally extended by Retry-After hint.
    ///
    /// If `retry_after_hint` is provided, waits for _both_ the hint duration _and_ the backoff
    /// to complete, ensuring we wait at least `max(hint, backoff_duration)`.
    async fn wait_for_backoff(
        &mut self,
        backoff: &mut exp_backoff::ExponentialBackoffStream,
        retry_after_hint: Option<Duration>,
    ) -> Result<(), ()> {
        if let Some(hint) = retry_after_hint {
            debug_assert!(
                hint <= self.max_duration,
                "hint {hint:?} exceeds max_duration {:?} -- take_combined_hint should have clamped",
                self.max_duration
            );
            tracing::info!(?hint, "Using Retry-After hint as backoff floor");

            let hint_sleep = tokio::time::sleep(hint);
            tokio::pin!(hint_sleep);

            let mut backoff_done = false;
            let mut hint_done = false;

            // Wait for both to complete
            loop {
                tokio::select! {
                    _ = backoff.next(), if !backoff_done => {
                        backoff_done = true;
                        if hint_done { break; }
                    }
                    _ = &mut hint_sleep, if !hint_done => {
                        hint_done = true;
                        if backoff_done { break; }
                    }
                    // Drain responses while the breaker is shut, but stop when
                    // the channel closes. recv() on a closed channel returns
                    // None at once, so looping on it would spin this task.
                    rsp = self.rsps.recv() => { rsp.ok_or(())?; continue }
                    _ = self.gate.lost() => return Err(()),
                }
            }
        } else {
            // No hint, use normal backoff
            loop {
                tokio::select! {
                    _ = backoff.next() => break,
                    // Drain responses while the breaker is shut, but stop when
                    // the channel closes. recv() on a closed channel returns
                    // None at once, so looping on it would spin this task.
                    rsp = self.rsps.recv() => { rsp.ok_or(())?; continue }
                    _ = self.gate.lost() => return Err(()),
                }
            }
        }

        Ok(())
    }

    /// Probation state: allow single probe request.
    ///
    /// Uses `gate.limit(1)` to allow exactly one request through.
    async fn probation(&mut self) -> Result<classify::Class, ()> {
        tracing::debug!("Probation");

        let _sem = self.gate.limit(1).map_err(|_| ())?;

        tokio::select! {
            rsp = self.rsps.recv() => rsp.ok_or(()),
            _ = self.gate.lost() => Err(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linkerd_proxy_client_policy::DEFAULT_RETRY_AFTER_MAX_DURATION;
    use tokio::time;
    use tokio_test::{assert_pending, task};

    fn make_backoff() -> ExponentialBackoff {
        ExponentialBackoff::try_new(
            time::Duration::from_secs(1),
            time::Duration::from_secs(100),
            0.0, // No jitter for deterministic tests
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

    fn default_test_config(
        gate_tx: gate::Tx,
        rsps: mpsc::Receiver<classify::Class>,
    ) -> UnifiedBreakerConfig {
        UnifiedBreakerConfig {
            max_failures: 3,
            threshold: 0.8,
            decay: time::Duration::from_secs(10),
            backoff: make_backoff(),
            min_requests: 1,
            gate: gate_tx,
            rsps,
            retry_after_store: RetryAfterStore::new(),
            grpc_retry_pushback_store: GrpcRetryPushbackStore::new(),
            max_duration: DEFAULT_RETRY_AFTER_MAX_DURATION,
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn starts_open() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            ..default_test_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());
        assert!(params.gate.is_open());
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn trips_on_consecutive_failures() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            ..default_test_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());
        assert!(params.gate.is_open());

        // Send 3 consecutive 5xx (should trip)
        for i in 1..=3 {
            send_err(&params, http::StatusCode::BAD_GATEWAY);
            time::advance(time::Duration::from_millis(100)).await;
            assert_pending!(task.poll());

            if i < 3 {
                assert!(
                    params.gate.is_open(),
                    "Should still be open after {} failures",
                    i
                );
            }
        }

        assert!(params.gate.is_shut(), "Should trip after 3 consecutive 5xx");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn success_resets_consecutive_count() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            ..default_test_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // Send 2 failures
        send_err(&params, http::StatusCode::BAD_GATEWAY);
        time::advance(time::Duration::from_millis(100)).await;
        assert_pending!(task.poll());

        send_err(&params, http::StatusCode::BAD_GATEWAY);
        time::advance(time::Duration::from_millis(100)).await;
        assert_pending!(task.poll());
        assert!(params.gate.is_open());

        // Success resets count
        send_ok(&params, http::StatusCode::OK);
        time::advance(time::Duration::from_millis(100)).await;
        assert_pending!(task.poll());

        // 2 more failures - still need 3 consecutive
        send_err(&params, http::StatusCode::BAD_GATEWAY);
        time::advance(time::Duration::from_millis(100)).await;
        assert_pending!(task.poll());

        send_err(&params, http::StatusCode::BAD_GATEWAY);
        time::advance(time::Duration::from_millis(100)).await;
        assert_pending!(task.poll());

        assert!(
            params.gate.is_open(),
            "Should still be open - only 2 consecutive after reset"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn trips_on_low_success_rate() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            max_failures: 100, // High max_failures so CF doesn't trip
            ..default_test_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // Send 1 success
        send_ok(&params, http::StatusCode::OK);
        time::advance(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());

        // Send 429s to drop success rate below 80%
        // With EWMA, after ~3-4 429s the rate drops below 0.8
        for _ in 0..4 {
            send_ok(&params, http::StatusCode::TOO_MANY_REQUESTS);
            time::advance(time::Duration::from_secs(1)).await;
            assert_pending!(task.poll());
        }

        assert!(!params.gate.is_open(), "Should trip on low success rate");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn success_rate_counts_5xx() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            max_failures: 100, // High max_failures so CF doesn't trip first
            ..default_test_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // 5xx should count as failure for success rate
        for _ in 0..5 {
            send_err(&params, http::StatusCode::BAD_GATEWAY);
            time::advance(time::Duration::from_secs(1)).await;
            assert_pending!(task.poll());
        }

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
            max_failures: 100, // High max_failures
            min_requests: 5,   // min_requests = 5
            ..default_test_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // Send 4 429s - should NOT trip (only 4 requests, need 5)
        for _ in 0..4 {
            send_ok(&params, http::StatusCode::TOO_MANY_REQUESTS);
            time::advance(time::Duration::from_secs(1)).await;
            assert_pending!(task.poll());
        }

        assert!(
            params.gate.is_open(),
            "Cold-start should protect success rate"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn probation_success_reopens() {
        let _trace = linkerd_tracing::test::trace_init();

        let (mut params, gate_tx, rsps) = gate::Params::channel(1);

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            ..default_test_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // Trip the breaker
        for _ in 0..3 {
            send_err(&params, http::StatusCode::BAD_GATEWAY);
            time::advance(time::Duration::from_millis(100)).await;
            assert_pending!(task.poll());
        }
        assert!(params.gate.is_shut());

        // Wait for backoff
        time::sleep(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());

        // Enter probation
        match params.gate.state() {
            gate::State::Limited(sem) => {
                assert_eq!(sem.available_permits(), 1);
                params.gate.opened_for_test().await.unwrap().forget();
            }
            _ => panic!("Expected Limited state"),
        }

        // Probe succeeds
        send_ok(&params, http::StatusCode::OK);
        assert_pending!(task.poll());
        assert!(params.gate.is_open());
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn probation_failure_on_429() {
        let _trace = linkerd_tracing::test::trace_init();

        let (mut params, gate_tx, rsps) = gate::Params::channel(1);

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            ..default_test_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // Trip the breaker
        for _ in 0..3 {
            send_err(&params, http::StatusCode::BAD_GATEWAY);
            time::advance(time::Duration::from_millis(100)).await;
            assert_pending!(task.poll());
        }
        assert!(params.gate.is_shut());

        // Wait for backoff and enter probation
        time::sleep(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());

        match params.gate.state() {
            gate::State::Limited(_) => {
                params.gate.opened_for_test().await.unwrap().forget();
            }
            _ => panic!("Expected Limited state"),
        }

        // Probe "fails" with 429 - strict AND recovery (dual mode, min_requests=1)
        send_ok(&params, http::StatusCode::TOO_MANY_REQUESTS);
        assert_pending!(task.poll());
        assert!(
            params.gate.is_shut(),
            "429 during probation should fail (strict AND)"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn retry_after_hint_extends_backoff() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);
        let retry_after_store = RetryAfterStore::new();

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            retry_after_store: retry_after_store.clone(),
            ..default_test_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // Record Retry-After hint of 5s (longer than 1s backoff)
        retry_after_store.record(time::Duration::from_secs(5));

        // Trip the breaker
        for _ in 0..3 {
            send_err(&params, http::StatusCode::BAD_GATEWAY);
            time::advance(time::Duration::from_millis(100)).await;
            assert_pending!(task.poll());
        }
        assert!(params.gate.is_shut());

        // After 1s (normal backoff would complete), still shut due to hint
        time::sleep(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());
        assert!(params.gate.is_shut());

        // After 3s more (4s total), still shut
        time::sleep(time::Duration::from_secs(3)).await;
        assert_pending!(task.poll());
        assert!(params.gate.is_shut());

        // After 1s more (5s total), enter probation
        time::sleep(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());
        assert!(params.gate.is_limited());
    }

    // Verify that consecutive-only mode (min_requests == usize::MAX) uses
    // class.is_success() for probe checks, preserving the old breaker behavior
    // where 429 is evaluated by the default classifier.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn consecutive_only_429_probe_preserves_old_semantics() {
        let _trace = linkerd_tracing::test::trace_init();

        let (mut params, gate_tx, rsps) = gate::Params::channel(1);

        // Consecutive-only mode: min_requests = usize::MAX disables EWMA tripping
        // and triggers the class.is_success() path in is_probe_success().
        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            min_requests: usize::MAX,
            threshold: 0.0, // threshold=0.0 also prevents SR trip
            ..default_test_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // Trip the breaker via consecutive failures
        for _ in 0..3 {
            send_err(&params, http::StatusCode::BAD_GATEWAY);
            time::advance(time::Duration::from_millis(100)).await;
            assert_pending!(task.poll());
        }
        assert!(params.gate.is_shut());

        // Wait for backoff and enter probation
        time::sleep(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());

        match params.gate.state() {
            gate::State::Limited(_) => {
                params.gate.opened_for_test().await.unwrap().forget();
            }
            _ => panic!("Expected Limited state"),
        }

        // In consecutive-only mode, 429 probe uses class.is_success().
        // The default HTTP classifier treats 429 as success (only 500-599 are
        // failures by default). So the probe should succeed and reopen the gate.
        send_ok(&params, http::StatusCode::TOO_MANY_REQUESTS);
        assert_pending!(task.poll());
        assert!(
            params.gate.is_open(),
            "In consecutive-only mode, 429 during probation should succeed \
             (delegates to class.is_success() which treats 429 as success)"
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
            threshold: 0.1,    // very low threshold so SR doesn't trip
            min_requests: 100, // high min_requests so SR doesn't trip
            ..default_test_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // RESOURCE_EXHAUSTED should not count as consecutive failures
        for _ in 0..5 {
            send_grpc(&params, tonic::Code::ResourceExhausted);
            time::advance(time::Duration::from_millis(100)).await;
            assert_pending!(task.poll());
        }

        assert!(
            params.gate.is_open(),
            "gRPC RESOURCE_EXHAUSTED should not trigger consecutive failures trip"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn trips_on_low_success_rate_grpc_resource_exhausted() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            max_failures: 100, // high max_failures so CF doesn't trip
            ..default_test_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // Send one success
        send_grpc(&params, tonic::Code::Ok);
        time::advance(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());

        // Send RESOURCE_EXHAUSTED to drop success rate below 80%
        for _ in 0..4 {
            send_grpc(&params, tonic::Code::ResourceExhausted);
            time::advance(time::Duration::from_secs(1)).await;
            assert_pending!(task.poll());
        }

        assert!(
            !params.gate.is_open(),
            "Should trip on low success rate from gRPC RESOURCE_EXHAUSTED"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn trips_on_low_success_rate_grpc_server_error() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            max_failures: 100,
            ..default_test_config(gate_tx, rsps)
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
            "Should trip on low success rate from gRPC Internal errors"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn probation_success_on_grpc_ok() {
        let _trace = linkerd_tracing::test::trace_init();

        let (mut params, gate_tx, rsps) = gate::Params::channel(1);

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            ..default_test_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // Trip the breaker
        for _ in 0..3 {
            send_err(&params, http::StatusCode::BAD_GATEWAY);
            time::advance(time::Duration::from_millis(100)).await;
            assert_pending!(task.poll());
        }
        assert!(params.gate.is_shut());

        // Wait for backoff
        time::sleep(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());

        // Enter probation
        match params.gate.state() {
            gate::State::Limited(sem) => {
                assert_eq!(sem.available_permits(), 1);
                params.gate.opened_for_test().await.unwrap().forget();
            }
            _ => panic!("Expected Limited state"),
        }

        // Probe succeeds with gRPC Ok
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
            threshold: 0.1,    // Very low threshold so SR doesn't trip
            min_requests: 100, // High min_requests so SR doesn't trip
            ..default_test_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // 429s should not count as consecutive failures for the CF policy
        for _ in 0..5 {
            send_ok(&params, http::StatusCode::TOO_MANY_REQUESTS);
            time::advance(time::Duration::from_millis(100)).await;
            assert_pending!(task.poll());
        }

        assert!(
            params.gate.is_open(),
            "429s should not trigger consecutive failures trip"
        );
    }

    // Verify threshold=0.0 disables success-rate tripping
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn threshold_zero_disables_success_rate() {
        let _trace = linkerd_tracing::test::trace_init();
        let (params, gate_tx, rsps) = gate::Params::channel(1);

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            max_failures: 100, // High max_failures so CF won't trip
            threshold: 0.0,    // threshold=0.0 (SR disabled)
            min_requests: 1,   // Cold-start passes after 1 request
            ..default_test_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());
        assert!(params.gate.is_open());

        // 1 success to pass cold-start (min_requests=1)
        send_ok(&params, http::StatusCode::OK);
        time::advance(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());

        // Many 429s to drop success rate toward 0%.
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

    // Verify Duration::MAX hint is clamped to DEFAULT_RETRY_AFTER_MAX_DURATION
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn duration_max_hint_is_clamped() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);
        let retry_after_store = RetryAfterStore::new();

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            retry_after_store: retry_after_store.clone(),
            ..default_test_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // Record a Duration::MAX hint BEFORE tripping the breaker.
        retry_after_store.record(Duration::MAX);

        // Trip the breaker with 3 consecutive 5xx errors.
        for _ in 0..3 {
            send_err(&params, http::StatusCode::BAD_GATEWAY);
            time::advance(time::Duration::from_millis(100)).await;
            assert_pending!(task.poll());
        }
        assert!(params.gate.is_shut(), "Should trip after 3 consecutive 5xx");

        // After DEFAULT_RETRY_AFTER_MAX_DURATION (300s), the breaker should enter probation.
        time::sleep(DEFAULT_RETRY_AFTER_MAX_DURATION).await;
        assert_pending!(task.poll());

        assert!(
            params.gate.is_limited(),
            "Duration::MAX hint should be clamped to DEFAULT_RETRY_AFTER_MAX_DURATION; \
             gate should enter probation after 300s, not sleep forever"
        );
    }

    // A closed response channel must end the breaker task even while it is
    // parked in the backoff wait. recv() on a closed channel yields None right
    // away, so the drain step has to treat None as teardown rather than looping
    // on it. The gate receiver is kept alive here, so gate.lost() never fires
    // and the task ends only through the closed-channel check.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn wait_for_backoff_terminates_when_channel_closes() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);

        // max_failures=1 trips the breaker on the first 5xx, dropping it into
        // the closed state and the backoff wait.
        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            max_failures: 1,
            ..default_test_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());
        assert!(params.gate.is_open());

        send_err(&params, http::StatusCode::INTERNAL_SERVER_ERROR);
        assert_pending!(task.poll());
        assert!(params.gate.is_shut(), "single 5xx should trip the breaker");

        // Close the channel while the breaker waits out the backoff. The task
        // must finish rather than spin on None.
        drop(params.responses);
        assert!(
            task.poll().is_ready(),
            "breaker must terminate when the response channel closes during backoff"
        );
    }

    // Same teardown as above, but a second gate receiver outlives the endpoint
    // service so gate.lost() can never resolve. Without the closed-channel
    // check the backoff wait spins forever on None. With it the task exits. A
    // real task is needed so the Gate plus BroadcastClassification stack can
    // drive a classification through the channel.
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
            ..default_test_config(gate_tx, rsps)
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
        // Dropping the response emits a final classification, then drops the
        // last sender so the channel closes.
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
    // `gate.limit(1)`, so the semaphore offers exactly one permit. Acquiring it
    // models the one admitted probe. A second concurrent request finds no permit
    // left and would block until the gate state changes.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn probation_admits_single_probe() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            ..default_test_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // Trip via 3 consecutive 5xx.
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

    // After a success-rate trip, an optimistic reopen plus steady healthy
    // traffic raises the EWMA above the threshold. A single failure right
    // after recovery loses its weight as successes add up across the decay
    // window, so the circuit stays open rather than tripping again on stale memory.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn success_rate_recovers_after_decay() {
        let _trace = linkerd_tracing::test::trace_init();

        let (mut params, gate_tx, rsps) = gate::Params::channel(1);

        // Consecutive failures disabled so only success rate drives the trip
        // and the later recovery. decay=10s keeps the arithmetic readable.
        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            max_failures: 0,
            threshold: 0.5,
            min_requests: 1,
            decay: time::Duration::from_secs(10),
            ..default_test_config(gate_tx, rsps)
        });
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // Seed one success so cold-start clears and the EWMA sits at 1.0.
        send_ok(&params, http::StatusCode::OK);
        time::advance(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());

        // 429s lower the rate below 0.5 and trip the circuit. With 1s spacing
        // the EWMA crosses 0.5 around the seventh sample. Stop at the trip so
        // the backoff timer is not advanced while the rate is still dropping.
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
            "successful probe reopens the circuit"
        );

        // One failure after recovery dips the newly reset EWMA but stays above
        // 0.5 (one zero against an optimistic 1.0).
        send_err(&params, http::StatusCode::BAD_GATEWAY);
        time::advance(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());
        assert!(
            params.gate.is_open(),
            "a single failure after recovery should not re-trip",
        );

        // Steady successes across two decay windows pull the rate back toward
        // 1.0 while the old failure decays out. The circuit holds open throughout.
        for _ in 0..20 {
            send_ok(&params, http::StatusCode::OK);
            time::advance(time::Duration::from_secs(1)).await;
            assert_pending!(task.poll());
            assert!(
                params.gate.is_open(),
                "recovered success rate should keep the circuit open",
            );
        }
    }

    // With both policies enabled, a stream of 429s lowers the success rate
    // without ever incrementing the consecutive counter (429 is not a hard
    // failure). The circuit trips on low success rate while consecutive failures
    // stay at zero, and the charged reason reflects that.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn dual_mode_success_rate_trips_before_consecutive() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);
        let (trip_tx, mut trip_rx) = mpsc::unbounded_channel();

        // Both mechanisms active: a five-failure consecutive budget and an 80%
        // success-rate floor with a three-request cold-start.
        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            max_failures: 5,
            threshold: 0.8,
            min_requests: 3,
            decay: time::Duration::from_secs(10),
            ..default_test_config(gate_tx, rsps)
        })
        .with_trip_observer(trip_tx);
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // Seed one success so the EWMA starts at 1.0 and the cold-start counter
        // begins climbing.
        send_ok(&params, http::StatusCode::OK);
        time::advance(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());

        // 429s push the rate under 0.8. Consecutive failures never move because
        // 429 is treated as success by the consecutive policy. With 1s spacing
        // the EWMA reads ~0.74 after the third 429, below 0.8, and min_requests
        // is met, so the trip happens there. Break at that response to check while
        // the gate is shut. A further 429 would advance time past the base
        // backoff, reopening the gate for a probe and hiding the shut state.
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
        // The reason proves the consecutive policy did not trip. `should_trip`
        // checks consecutive failures first, so a `LowSuccessRate` verdict means
        // the consecutive counter never reached max_failures.
        assert_eq!(
            trip_rx.try_recv(),
            Ok(TripReason::LowSuccessRate),
            "the trip is attributed to low success rate, not consecutive failures",
        );
    }

    // A consecutive-failure trip in a single-policy configuration reports
    // `ConsecutiveFailures`. Success rate is disabled (threshold 0.0), so the
    // only path to a trip is the consecutive counter reaching max_failures.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn consecutive_trip_reports_consecutive_reason() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);
        let (trip_tx, mut trip_rx) = mpsc::unbounded_channel();

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            max_failures: 3,
            threshold: 0.0, // success rate disabled
            min_requests: usize::MAX,
            ..default_test_config(gate_tx, rsps)
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

    // A success-rate trip in a single-policy configuration reports
    // `LowSuccessRate`. Consecutive failures are disabled (max_failures 0), so
    // only EWMA degradation can trip.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn success_rate_trip_reports_low_success_rate_reason() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);
        let (trip_tx, mut trip_rx) = mpsc::unbounded_channel();

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            max_failures: 0, // consecutive failures disabled
            threshold: 0.5,
            min_requests: 1,
            decay: time::Duration::from_secs(10),
            ..default_test_config(gate_tx, rsps)
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

    // When one response crosses both thresholds at once, consecutive failures
    // win the charge because `should_trip` checks them first. The config is tuned
    // so the third 5xx reaches max_failures, satisfies min_requests, and drops
    // the EWMA below the threshold on the same response.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn simultaneous_threshold_cross_reports_consecutive_reason() {
        let _trace = linkerd_tracing::test::trace_init();

        let (params, gate_tx, rsps) = gate::Params::channel(1);
        let (trip_tx, mut trip_rx) = mpsc::unbounded_channel();

        let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
            max_failures: 3,
            threshold: 0.8,
            min_requests: 3,
            decay: time::Duration::from_secs(10),
            ..default_test_config(gate_tx, rsps)
        })
        .with_trip_observer(trip_tx);
        let mut task = task::spawn(breaker.run());

        assert_pending!(task.poll());

        // 1s spacing decays the EWMA fast. After three 5xx it reads ~0.74, below
        // 0.8, exactly as the consecutive counter reaches 3 and min_requests is
        // met. Both conditions hold on the third response.
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
}
