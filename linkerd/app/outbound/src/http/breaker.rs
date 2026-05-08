use linkerd_app_core::{classify, proxy::http::classify::gate, svc};
use linkerd_proxy_client_policy::FailureAccrual;

use tokio::time::Duration;
use tracing::{trace_span, Instrument};

mod consecutive_failures;
pub mod retry_after;
mod unified;
pub mod wrap_classify;

use self::consecutive_failures::ConsecutiveFailures;
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
        let (prms, gate, rsps) = gate::Params::channel(self.channel_capacity);

        match self.accrual {
            None => {
                // No failure accrual for this target; construct a gate
                // that will never close.
                tracing::trace!("No failure accrual policy enabled.");
                prms
            }
            Some(ref accrual) => {
                let max_failures = accrual.consecutive.max_failures;
                let backoff = accrual.consecutive.backoff;
                tracing::trace!(
                    max_failures,
                    backoff = ?backoff,
                    "Using consecutive-failures failure accrual policy.",
                );

                // 1. If the configured number of consecutive failures are encountered,
                //    shut the gate.
                // 2. After an ejection timeout, open the gate so that 1 request can be processed.
                // 3. If that request succeeds, open the gate. If it fails, increase the
                //    ejection timeout and repeat.
                let breaker = ConsecutiveFailures::new(max_failures, backoff, gate, rsps);
                tokio::spawn(
                    breaker
                        .run()
                        .instrument(trace_span!("consecutive_failures").or_current()),
                );

                prms
            }
        }
    }
}
