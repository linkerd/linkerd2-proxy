use linkerd_app_core::{classify, proxy::http::classify::gate, svc};
use std::{fmt::Debug, pin::Pin, sync::Arc};
use tokio::{
    sync::{mpsc, Semaphore},
    time,
};

#[derive(Copy, Clone, Debug)]
pub struct Params {
    pub max_failures: usize,
    pub channel_capacity: usize,
    // TODO Use `ExponentialBackoff`
    pub init_ejection_backoff: time::Duration,
    pub max_ejection_backoff: time::Duration,
}

pub struct ConsecutiveFailures {
    params: Params,
    gate: gate::Tx,
    rsps: mpsc::Receiver<classify::Class>,
    semaphore: Arc<Semaphore>,
    sleep: Pin<Box<time::Sleep>>,
}

// === impl Params ===

impl<T> svc::ExtractParam<gate::Params<classify::Class>, T> for Params {
    fn extract_param(&self, _: &T) -> gate::Params<classify::Class> {
        // Create a channel so that we can receive response summaries and
        // control the gate.
        let (prms, gate, rsps) = gate::Params::channel(self.channel_capacity);

        // 1. If the configured number of consecutive failures are encountered,
        //    shut the gate.
        // 2. After an ejection timeout, open the gate so that 1 request can be processed.
        // 3. If that request succeeds, open the gate. If it fails, increase the
        //    ejection timeout and repeat.
        let breaker = ConsecutiveFailures::new(*self, gate, rsps);
        tokio::spawn(breaker.run());

        prms
    }
}

// === impl ConsecutiveFailures ===

impl ConsecutiveFailures {
    pub fn new(params: Params, gate: gate::Tx, rsps: mpsc::Receiver<classify::Class>) -> Self {
        Self {
            params,
            gate,
            rsps,
            sleep: Box::pin(tokio::time::sleep(time::Duration::from_secs(0))),
            semaphore: Arc::new(Semaphore::new(0)),
        }
    }

    async fn run(mut self) {
        loop {
            if self.open().await.is_err() {
                return;
            }

            if self.closed().await.is_err() {
                return;
            }
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
                if failures == self.params.max_failures {
                    return Ok(());
                }
            }
        }
    }

    /// Keep the breaker closed for at least `init_ejection_backoff` and then,
    /// once the timeout expires, go into probation to admit a single request
    /// before reverting to the open state or continuing in the shut state.
    async fn closed(&mut self) -> Result<(), ()> {
        let mut backoff = self.params.init_ejection_backoff;
        loop {
            // The breaker is shut now. Wait until we can open it again.
            tracing::debug!(backoff = ?backoff, "Shut");
            self.gate.shut();

            self.sleep.as_mut().reset(time::Instant::now() + backoff);
            loop {
                tokio::select! {
                    _ = &mut self.sleep => break,
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

            // Re-enter the shut state with the next backoff.
            backoff = self.params.max_ejection_backoff.min(backoff * 2);
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

        let prms = Params {
            max_failures: 2,
            channel_capacity: 1,
            init_ejection_backoff: time::Duration::from_secs(1),
            max_ejection_backoff: time::Duration::from_secs(100),
        };
        let breaker = ConsecutiveFailures::new(prms, gate, rsps);
        let mut task = task::spawn(breaker.run());

        // Start open and failing.
        send(Err(http::StatusCode::BAD_GATEWAY));
        assert_pending!(task.poll());
        assert!(params.gate.is_open());

        send(Err(http::StatusCode::BAD_GATEWAY));
        assert_pending!(task.poll());
        assert!(params.gate.is_shut());

        // After two failures, we've the breaker is closed.
        time::sleep(time::Duration::from_millis(500)).await;
        assert_pending!(task.poll());
        assert!(params.gate.is_shut());

        // It remains closed until the init ejection backoff elapses. Then it's
        // limited to a single request.
        time::sleep(time::Duration::from_millis(500)).await;
        assert_pending!(task.poll());
        match params.gate.state() {
            gate::State::Open => panic!("still open"),
            gate::State::Shut => panic!("still shut"),
            gate::State::Limited(sem) => {
                assert_eq!(sem.available_permits(), 1);
                params.gate.acquire().await.unwrap().forget();
                assert_eq!(sem.available_permits(), 0);
            }
        }

        // The first request in probation fails, so the break remains closed.
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
                params.gate.acquire().await.unwrap().forget();
                assert_eq!(sem.available_permits(), 0);
            }
        }

        // And then a single success takes us back into the open state!
        send(Ok(http::StatusCode::OK));
        assert_pending!(task.poll());
        assert!(params.gate.is_open());
    }
}
