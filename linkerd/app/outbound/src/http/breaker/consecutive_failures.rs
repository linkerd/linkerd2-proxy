use futures::stream::StreamExt;
use linkerd_app_core::{classify, exp_backoff::ExponentialBackoff, proxy::http::classify::gate};
use std::sync::Arc;
use tokio::sync::{mpsc, Semaphore};

pub struct ConsecutiveFailures {
    max_failures: usize,
    backoff: ExponentialBackoff,
    gate: gate::Tx,
    rsps: mpsc::Receiver<classify::Class>,
    semaphore: Arc<Semaphore>,
}

impl ConsecutiveFailures {
    pub fn new(
        max_failures: usize,
        backoff: ExponentialBackoff,
        gate: gate::Tx,
        rsps: mpsc::Receiver<classify::Class>,
    ) -> Self {
        Self {
            max_failures,
            backoff,
            gate,
            rsps,
            semaphore: Arc::new(Semaphore::new(0)),
        }
    }

    pub(super) async fn run(mut self) {
        loop {
            if self.open().await.is_err() {
                return;
            }

            tracing::info!("Consecutive failure-accrual breaker closed");
            if self.closed().await.is_err() {
                return;
            }

            tracing::info!("Consecutive failure-accrual breaker reopened");
        }
    }

    /// Keep the breaker open until `max_failures` consecutive failures are
    /// observed.
    async fn open(&mut self) -> Result<(), ()> {
        tracing::debug!("Open");
        self.gate.open();
        let mut failures = 0;
        loop {
            let class = tokio::select! {
                rsp = self.rsps.recv() => rsp.ok_or(())?,
                _ = self.gate.lost() => return Err(()),
            };

            tracing::trace!(?class, %failures, "Response");
            if class.is_success() {
                failures = 0;
            } else {
                failures += 1;
                if failures == self.max_failures {
                    return Ok(());
                }
            }
        }
    }

    /// Keep the breaker closed for at least the initial backoff, and then,
    /// once the timeout expires, go into probation to admit a single request
    /// before reverting to the open state or continuing in the shut state.
    async fn closed(&mut self) -> Result<(), ()> {
        let mut backoff = self.backoff.stream();
        loop {
            // The breaker is shut now. Wait until we can open it again.
            tracing::debug!(backoff = ?backoff.duration(), "Shut");
            self.gate.shut();

            loop {
                tokio::select! {
                    _ = backoff.next() => break,
                    // Ignore responses while the breaker is shut.
                    _ = self.rsps.recv() => continue,
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

    /// Wait for a response to determine whether the breaker should be opened.
    async fn probation(&mut self) -> Result<classify::Class, ()> {
        tracing::debug!("Probation");
        self.semaphore.add_permits(1);
        self.gate.limit(self.semaphore.clone());
        tokio::select! {
            rsp = self.rsps.recv() => rsp.ok_or(()),
            _ = self.gate.lost() => Err(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time;
    use tokio_test::{assert_pending, task};

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn transitions() {
        let _trace = linkerd_tracing::test::trace_init();

        let (mut params, gate, rsps) = gate::Params::channel(1);
        let send = |res: Result<http::StatusCode, http::StatusCode>| {
            params
                .responses
                .try_send(classify::Class::Http(res))
                .unwrap()
        };

        let backoff = ExponentialBackoff::try_new(
            time::Duration::from_secs(1),
            time::Duration::from_secs(100),
            // Don't jitter backoffs to ensure tests are deterministic.
            0.0,
        )
        .expect("backoff params are valid");
        let breaker = ConsecutiveFailures::new(2, backoff, gate, rsps);
        let mut task = task::spawn(breaker.run());

        // Start open and failing.
        send(Err(http::StatusCode::BAD_GATEWAY));
        assert_pending!(task.poll());
        assert!(params.gate.is_open());

        send(Err(http::StatusCode::BAD_GATEWAY));
        assert_pending!(task.poll());
        assert!(params.gate.is_shut());

        // After two failures, the breaker is closed.
        time::sleep(time::Duration::from_millis(500)).await;
        assert_pending!(task.poll());
        assert!(params.gate.is_shut());

        // It remains closed until the init ejection backoff elapses. Then it's
        // limited to a single request.
        time::sleep(time::Duration::from_millis(500)).await;
        assert_pending!(task.poll());

        // hold the permit to prevent the breaker from opening
        match params.gate.state() {
            gate::State::Open => panic!("still open"),
            gate::State::Shut => panic!("still shut"),
            gate::State::Limited(sem) => {
                assert_eq!(sem.available_permits(), 1);
                params
                    .gate
                    .acquire_for_test()
                    .await
                    .expect("permit should be acquired")
                    // The `Gate` service would forget this permit when called, so
                    // we must do the same here explicitly.
                    .forget();
                assert_eq!(sem.available_permits(), 0);
            }
        };

        // The first request in probation fails, so the breaker remains closed.
        send(Err(http::StatusCode::BAD_GATEWAY));
        assert_pending!(task.poll());
        assert!(params.gate.is_shut());

        // The breaker goes into closed for 2s now...
        time::sleep(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());
        assert!(params.gate.is_shut());

        // If some straggling responses are observed while the breaker is
        // closed, they are ignored.
        send(Ok(http::StatusCode::OK));
        assert_pending!(task.poll());
        assert!(params.gate.is_shut());
        // After that timeout elapses, we go into probation again.
        time::sleep(time::Duration::from_secs(1)).await;
        assert_pending!(task.poll());

        match params.gate.state() {
            gate::State::Open => panic!("still open"),
            gate::State::Shut => panic!("still shut"),
            gate::State::Limited(sem) => {
                assert_eq!(sem.available_permits(), 1);
                params
                    .gate
                    .acquire_for_test()
                    .await
                    .expect("permit should be acquired")
                    // The `Gate` service would forget this permit when called, so
                    // we must do the same here explicitly.
                    .forget();
                assert_eq!(sem.available_permits(), 0);
            }
        }

        // And then a single success takes us back into the open state!
        send(Ok(http::StatusCode::OK));
        assert_pending!(task.poll());
        assert!(params.gate.is_open());
    }
}
