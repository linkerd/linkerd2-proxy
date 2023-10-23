use linkerd_stack::gate;
use std::pin::Pin;
use tokio::time;

#[derive(Debug)]
pub(super) struct Failfast {
    timeout: time::Duration,
    sleep: Pin<Box<time::Sleep>>,
    state: Option<State>,
    gate: gate::Tx,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum State {
    Waiting { since: time::Instant },
    Failfast { since: time::Instant },
}

// === impl Failfast ===

impl Failfast {
    pub(super) fn new(timeout: time::Duration, gate: gate::Tx) -> Self {
        Self {
            timeout,
            sleep: Box::pin(time::sleep(time::Duration::MAX)),
            state: None,
            gate,
        }
    }

    /// Returns true if we are currently in a failfast state.
    pub(super) fn is_active(&self) -> bool {
        matches!(self.state, Some(State::Failfast { .. }))
    }

    /// Clears any waiting or failfast state.
    pub(super) fn set_ready(&mut self) {
        if let Some(state) = self.state.take() {
            self.gate.open();
            match state {
                State::Waiting { since } => {
                    tracing::debug!(
                        elapsed = (time::Instant::now() - since).as_secs_f64(),
                        "Ready"
                    );
                }
                State::Failfast { since } => {
                    tracing::info!(
                        elapsed = (time::Instant::now() - since).as_secs_f64(),
                        "Exited failfast"
                    );
                }
            };
        }
    }

    /// Waits for the failfast timeout to expire and enters the failfast state.
    pub(super) async fn timeout(&mut self) {
        let since = match self.state {
            // If we're already in failfast, then we don't need to wait.
            Some(State::Failfast { .. }) => {
                return;
            }

            // Ensure that the timer's been initialized.
            Some(State::Waiting { since }) => since,
            None => {
                let now = time::Instant::now();
                self.sleep.as_mut().reset(now + self.timeout);
                self.state = Some(State::Waiting { since: now });
                now
            }
        };

        // Wait for the failfast timer to expire.
        self.sleep.as_mut().await;

        // Once we enter failfast, shut the upstream gate so that we can
        // advertise backpressure past the queue.
        self.state = Some(State::Failfast { since });
        self.gate.shut();
    }
}
