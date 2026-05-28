use linkerd_app_core::{classify, proxy::http::classify::gate, svc};
use linkerd_proxy_client_policy::FailureAccrual;

use tokio::time::Duration;
use tracing::{trace_span, Instrument};

mod consecutive_failures;
pub mod retry_after;
mod unified;

use self::consecutive_failures::ConsecutiveFailures;
use self::unified::{UnifiedBreaker, UnifiedBreakerConfig};

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
#[derive(Clone, Debug)]
pub(crate) struct Params {
    pub(crate) accrual: Option<FailureAccrual>,
    pub(crate) channel_capacity: usize,
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
