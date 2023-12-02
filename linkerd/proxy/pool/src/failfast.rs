use linkerd_stack::gate;
use std::pin::Pin;
use tokio::time;

/// Manages the failfast state for a pool.
#[derive(Debug)]
pub(super) struct Failfast {
    timeout: time::Duration,
    sleep: Pin<Box<time::Sleep>>,
    state: Option<State>,
    gate: gate::Tx,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) enum State {
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

    pub(super) fn duration(&self) -> time::Duration {
        self.timeout
    }

    /// Returns true if we are currently in a failfast state.
    pub(super) fn is_active(&self) -> bool {
        matches!(self.state, Some(State::Failfast { .. }))
    }

    /// Clears any waiting or failfast state.
    pub(super) fn set_ready(&mut self) -> Option<State> {
        let state = self.state.take()?;
        if matches!(state, State::Failfast { .. }) {
            tracing::trace!("Exiting failfast");
            let _ = self.gate.open();
        }
        Some(state)
    }

    /// Waits for the failfast timeout to expire and enters the failfast state.
    pub(super) async fn entered(&mut self) {
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
        tracing::trace!("Waiting for failfast timeout");
        self.sleep.as_mut().await;
        tracing::trace!("Entering failfast");

        // Once we enter failfast, shut the upstream gate so that we can
        // advertise backpressure past the queue.
        self.state = Some(State::Failfast { since });
        let _ = self.gate.shut();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::prelude::*;

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn failfast() {
        let (tx, gate_rx) = gate::channel();
        let dur = time::Duration::from_secs(1);
        let mut failfast = Failfast::new(dur, tx);

        assert_eq!(dur, failfast.duration());
        assert!(gate_rx.is_open());

        // The failfast timeout should not be initialized until the first
        // request is received.
        assert!(!failfast.is_active(), "failfast should be active");

        failfast.entered().await;
        assert!(failfast.is_active(), "failfast should be active");
        assert!(gate_rx.is_shut(), "gate should be shut");

        failfast
            .entered()
            .now_or_never()
            .expect("timeout must return immediately when in failfast");
        assert!(failfast.is_active(), "failfast should be active");
        assert!(gate_rx.is_shut(), "gate should be shut");

        failfast.set_ready();
        assert!(!failfast.is_active(), "failfast should be inactive");
        assert!(gate_rx.is_open(), "gate should be open");

        tokio::select! {
            _ = time::sleep(time::Duration::from_millis(10)) => {}
            _ = failfast.entered() => unreachable!("timed out too quick"),
        }
        assert!(!failfast.is_active(), "failfast should be inactive");
        assert!(gate_rx.is_open(), "gate should be open");

        assert!(
            matches!(failfast.state, Some(State::Waiting { .. })),
            "failfast should be waiting"
        );

        failfast.entered().await;
        assert!(failfast.is_active(), "failfast should be active");
        assert!(gate_rx.is_shut(), "gate should be shut");
    }
}
