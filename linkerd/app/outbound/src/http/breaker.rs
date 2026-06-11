use linkerd_app_core::{classify, proxy::http::classify::gate, svc};
use linkerd_proxy_client_policy::FailureAccrual;
use tracing::{trace_span, Instrument};

mod consecutive_failures;
mod success_rate;
mod unified;

#[cfg(test)]
mod integration_tests;

use self::consecutive_failures::ConsecutiveFailures;
use self::unified::{UnifiedBreaker, UnifiedBreakerConfig};

/// Reason a circuit breaker tripped.
///
/// The unified breaker can trip on one of two conditions and reports which one
/// fired. The reason is reported for observability. Either condition opens the
/// circuit in the same way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TripReason {
    /// Tripped on a run of consecutive 5xx failures.
    ConsecutiveFailures,
    /// Tripped when the windowed success rate fell below the threshold.
    LowSuccessRate,
}

/// Params configuring a circuit breaker stack.
///
/// The outbound stack builds one set per endpoint. Each endpoint's breaker
/// tracks only its own failures. An absent policy disables the breaker.
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

        match &self.accrual {
            None => {
                // No failure accrual for this target; construct a gate
                // that will never close.
                tracing::trace!("No failure accrual policy enabled.");
                prms
            }
            Some(FailureAccrual::Consecutive(cf)) => {
                // Consecutive-only policy that trips after N consecutive
                // failures and probes leniently, so the default classifier
                // judges a 429. This breaker follows its plain exponential
                // backoff.
                tracing::trace!(
                    max_failures = cf.max_failures,
                    backoff = ?cf.backoff,
                    "Using consecutive-failures failure accrual policy.",
                );

                // 1. If the configured number of consecutive failures are encountered,
                //    shut the gate.
                // 2. After an ejection timeout, open the gate so that 1 request can be processed.
                // 3. If that request succeeds, open the gate. If it fails, increase the
                //    ejection timeout and repeat.
                let breaker = ConsecutiveFailures::new(cf.max_failures, cf.backoff, gate, rsps);
                tokio::spawn(
                    breaker
                        .run()
                        .instrument(trace_span!("consecutive_failures").or_current()),
                );

                prms
            }
            Some(FailureAccrual::Unified(u)) => {
                // Unified policy with a consecutive-failure ceiling and a
                // windowed success-rate threshold, either of which can trip. The
                // probe is strict, so a 429 keeps the circuit shut, and the
                // fraction is recovered from the basis-points threshold for the
                // engine.
                tracing::trace!(
                    max_consecutive_failures = u.max_consecutive_failures,
                    threshold = %u.threshold,
                    min_requests = u.min_requests,
                    backoff = ?u.backoff,
                    "Using unified failure accrual policy with consecutive failures and success rate tracking.",
                );

                let breaker = UnifiedBreaker::new(UnifiedBreakerConfig {
                    max_failures: u.max_consecutive_failures,
                    threshold: u.threshold.as_fraction(),
                    window: u.window,
                    backoff: u.backoff,
                    min_requests: u.min_requests as usize,
                    gate,
                    rsps,
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
