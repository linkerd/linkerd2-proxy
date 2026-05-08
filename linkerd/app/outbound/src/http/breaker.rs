//! Circuit breaker infrastructure for HTTP endpoints.
//!
//! This module implements a unified circuit breaker with two complementary failure
//! tracking mechanisms: consecutive failures and success rate (EWMA-based).
//!
//! The first trips the breaker after N consecutive failures (typically 5xx statuses).
//! It uses exponential backoff before allowing probe requests.
//!
//! The second trips when the success rate drops below a threshold. It treats both 5xx
//! and 429 as failures to enable rate limiting awareness.
//!
//! Both mechanisms are tracked by a single [`UnifiedBreaker`] policy, in which any
//! one of both conditions can trip the circuit.
//!
//! ## Recovery
//!
//! During probation, a probe request must be _non-5xx AND non-429_ to succeed.
//! This ensures the endpoint isn't still rate limiting before reopening.
//!
//! ## Retry-After Integration
//!
//! When a 429 response includes a `Retry-After` header, the duration is captured and used
//! to extend the circuit breaker's backoff period.
//!
//! ## Cold-start
//!
//! Cold-start protection applies to the success rate mechanism.
//!
//! ## Usage
//!
//! The breaker is integrated into the load balancer's endpoint stack via [`NewRetryAfterGateSet`],
//! which replaces the standard `NewClassifyGateSet` to add Retry-After extraction.

use linkerd_app_core::{classify, proxy::http::classify::gate, svc};
use linkerd_proxy_client_policy::FailureAccrual;

use tokio::time::Duration;
use tracing::{trace_span, Instrument};

pub mod retry_after;
mod unified;
pub mod wrap_classify;

pub use self::retry_after::{GrpcRetryPushbackStore, RetryAfterStore};
use self::unified::{UnifiedBreaker, UnifiedBreakerConfig};
pub use self::wrap_classify::{HasFailureAccrual, NewRetryAfterGateSet, RetryAfterGateParams};

/// Reason why the circuit breaker tripped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TripReason {
    /// Tripped due to N consecutive 5xx failures.
    ConsecutiveFailures,
    /// Tripped due to EWMA success rate dropping below threshold.
    LowSuccessRate,
}

/// Default EWMA decay window (used when success_rate is configured but has no explicit decay).
const DEFAULT_SUCCESS_RATE_DECAY: Duration = Duration::from_secs(10);

/// Params configuring a circuit breaker stack.
///
/// Retry-After stores move hints from response classification to the
/// circuit breaker backoff logic. A new pair is built for each endpoint and
/// shared between that endpoint's classifier and its [`UnifiedBreaker`], so a
/// hint seen on one endpoint never extends the backoff of another.
#[derive(Clone, Debug)]
pub(crate) struct Params {
    pub(crate) accrual: Option<FailureAccrual>,
    pub(crate) channel_capacity: usize,
    /// Shared store for HTTP Retry-After hints.
    pub(crate) retry_after_store: RetryAfterStore,
    /// Shared store for gRPC retry pushback hints.
    pub(crate) grpc_retry_pushback_store: GrpcRetryPushbackStore,
    /// Maximum Retry-After duration the proxy will honor (clamping cap).
    pub(crate) max_duration: Duration,
}

impl<T> svc::ExtractParam<gate::Params<classify::Class>, T> for Params {
    fn extract_param(&self, _: &T) -> gate::Params<classify::Class> {
        // Create a channel so that we can receive response summaries and
        // control the gate.
        let (prms, gate_tx, rsps) = gate::Params::channel(self.channel_capacity);

        match self.accrual {
            None => {
                // No failure accrual for this target; construct a gate
                // that will never close.
                tracing::trace!("No failure accrual policy enabled.");
                prms
            }
            Some(ref accrual) => {
                // When success_rate is Some, use configured EWMA policy values.
                // When None (consecutive-only mode), disable EWMA by using
                // threshold 0.0 (rate >= 0 always, so never trips) and
                // min_requests usize::MAX (cold-start never passes).
                let (success_rate_threshold, success_rate_decay, min_request_threshold) =
                    if let Some(sr) = accrual.success_rate {
                        if accrual.consecutive.max_failures == 0 {
                            tracing::trace!(
                                success_rate_threshold = %sr.threshold,
                                min_requests = sr.min_requests,
                                backoff = ?accrual.consecutive.backoff,
                                "Using circuit breaker with consecutive failures disabled and success rate tracking.",
                            );
                        } else {
                            tracing::trace!(
                                max_failures = accrual.consecutive.max_failures,
                                backoff = ?accrual.consecutive.backoff,
                                success_rate_threshold = %sr.threshold,
                                min_requests = sr.min_requests,
                                "Using unified circuit breaker with consecutive failures and success rate tracking.",
                            );
                        }
                        (sr.threshold, sr.decay, sr.min_requests as usize)
                    } else {
                        if accrual.consecutive.max_failures == 0 {
                            tracing::trace!(
                                backoff = ?accrual.consecutive.backoff,
                                "Using circuit breaker with consecutive failures disabled.",
                            );
                        } else {
                            tracing::trace!(
                                max_failures = accrual.consecutive.max_failures,
                                backoff = ?accrual.consecutive.backoff,
                                "Using circuit breaker with consecutive failures only.",
                            );
                        }
                        (0.0_f64, DEFAULT_SUCCESS_RATE_DECAY, usize::MAX)
                    };

                // Spawn the unified breaker that handles both policies
                let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
                    max_failures: accrual.consecutive.max_failures,
                    threshold: success_rate_threshold,
                    decay: success_rate_decay,
                    backoff: accrual.consecutive.backoff,
                    min_requests: min_request_threshold,
                    gate: gate_tx,
                    rsps,
                    retry_after_store: self.retry_after_store.clone(),
                    grpc_retry_pushback_store: self.grpc_retry_pushback_store.clone(),
                    max_duration: self.max_duration,
                });

                tokio::spawn(
                    breaker
                        .run()
                        .instrument(trace_span!("unified_breaker").or_current()),
                );

                prms
            }
        }
    }
}
