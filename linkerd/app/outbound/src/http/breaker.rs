mod consecutive_failures;
use consecutive_failures::ConsecutiveFailures;
use linkerd_app_core::{classify, proxy::http::classify::gate, svc};
use linkerd_proxy_client_policy::FailureAccrual;

/// Params configuring a circuit breaker stack.
#[derive(Copy, Clone, Debug)]
pub(crate) struct Params {
    pub(crate) accrual: FailureAccrual,
    pub(crate) channel_capacity: usize,
}

impl<T> svc::ExtractParam<gate::Params<classify::Class>, T> for Params {
    fn extract_param(&self, _: &T) -> gate::Params<classify::Class> {
        // Create a channel so that we can receive response summaries and
        // control the gate.
        let (prms, gate, rsps) = gate::Params::channel(self.channel_capacity);

        match self.accrual {
            FailureAccrual::None => {
                // No failure accrual for this target; construct a gate
                // that will never close.
                tracing::trace!("No failure accrual policy enabled.");
                prms
            }
            FailureAccrual::ConsecutiveFailures {
                max_failures,
                backoff,
            } => {
                tracing::trace!(max_failures, backoff = ?backoff, "Using consecutive-failures failure accrual policy.");

                // 1. If the configured number of consecutive failures are encountered,'
                //    shut the gate.
                // 2. After an ejection timeout, open the gate so that 1 request can be processed.
                // 3. If that request succeeds, open the gate. If it fails, increase the
                //    ejection timeout and repeat.
                let breaker = ConsecutiveFailures::new(max_failures, backoff, gate, rsps);
                tokio::spawn(breaker.run());

                prms
            }
        }
    }
}
