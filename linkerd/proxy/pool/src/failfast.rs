use linkerd_metrics::prom;
use linkerd_stack::gate;
use std::{
    pin::Pin,
    sync::atomic::AtomicU64,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::time;

/// Manages the failfast state for a pool.
#[derive(Debug)]
pub(super) struct Failfast {
    timeout: time::Duration,
    sleep: Pin<Box<time::Sleep>>,
    state: Option<State>,
    metrics: GateMetrics,
    gate: gate::Tx,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) enum State {
    Waiting { since: time::Instant },
    Failfast { since: time::Instant },
}

#[derive(Clone, Debug)]
pub(crate) struct GateMetricFamilies<L> {
    changed_time: prom::Family<GateLabel<L>, prom::Gauge<f64, AtomicU64>>,
    changes: prom::Family<GateLabel<L>, prom::Counter>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct GateMetrics {
    open: prom::Counter,
    close: prom::Counter,
    opened_time: prom::Gauge<f64, AtomicU64>,
    closed_time: prom::Gauge<f64, AtomicU64>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
struct GateLabel<L> {
    state: GateState,
    parent: L,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
enum GateState {
    Open,
    Closed,
}

// === impl Failfast ===

impl Failfast {
    pub(super) fn new(timeout: time::Duration, gate: gate::Tx, metrics: GateMetrics) -> Self {
        Self {
            timeout,
            sleep: Box::pin(time::sleep(time::Duration::MAX)),
            state: None,
            metrics,
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
            self.metrics.open.inc();
            self.metrics.opened_time.set(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_else(|_| Default::default())
                    .as_secs_f64(),
            );
            self.metrics.closed_time.set(0.0);
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
        self.metrics.close.inc();
        self.metrics.closed_time.set(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_else(|_| Default::default())
                .as_secs_f64(),
        );
        self.metrics.opened_time.set(0.0);
    }
}

impl<L> GateMetricFamilies<L>
where
    L: prom::encoding::EncodeLabelSet + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone + Send + Sync + 'static,
{
    pub(crate) fn register(reg: &mut prom::Registry) -> Self {
        let changes = prom::Family::default();
        reg.register(
            "state_changes",
            "The total number of gate state changes between open and closed",
            changes.clone(),
        );

        let changed_time = prom::Family::default();
        reg.register(
            "changed_time",
            "Indicates the time of the last state change for the current gate state",
            changed_time.clone(),
        );

        Self {
            changes,
            changed_time,
        }
    }

    pub(crate) fn metrics(&self, labels: &L) -> GateMetrics {
        let open = self
            .changes
            .get_or_create(&GateLabel {
                state: GateState::Open,
                parent: labels.clone(),
            })
            .clone();
        let close = self
            .changes
            .get_or_create(&GateLabel {
                state: GateState::Closed,
                parent: labels.clone(),
            })
            .clone();
        let opened_time = self
            .changed_time
            .get_or_create(&GateLabel {
                state: GateState::Open,
                parent: labels.clone(),
            })
            .clone();
        let closed_time = self
            .changed_time
            .get_or_create(&GateLabel {
                state: GateState::Closed,
                parent: labels.clone(),
            })
            .clone();
        GateMetrics {
            open,
            close,
            opened_time,
            closed_time,
        }
    }
}

impl<L: prom::encoding::EncodeLabelSet> prom::encoding::EncodeLabelSet for GateLabel<L> {
    fn encode(&self, mut enc: prom::encoding::LabelSetEncoder<'_>) -> std::fmt::Result {
        use prom::encoding::EncodeLabel;
        ("state", self.state.as_str()).encode(enc.encode_label())?;
        self.parent.encode(enc)
    }
}

impl GateState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
        }
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
        let metrics = GateMetrics::default();
        let mut failfast = Failfast::new(dur, tx, metrics.clone());

        assert_eq!(dur, failfast.duration());
        assert!(gate_rx.is_open());

        // The failfast timeout should not be initialized until the first
        // request is received.
        assert!(!failfast.is_active(), "failfast should be active");

        assert_eq!(metrics.open.get(), 0);
        assert_eq!(metrics.close.get(), 0);
        assert_eq!(metrics.opened_time.get(), 0.0);
        assert_eq!(metrics.closed_time.get(), 0.0);

        failfast.entered().await;
        assert!(failfast.is_active(), "failfast should be active");
        assert!(gate_rx.is_shut(), "gate should be shut");

        assert_eq!(metrics.open.get(), 0);
        assert_eq!(metrics.close.get(), 1);
        assert_eq!(metrics.opened_time.get(), 0.0);
        assert_ne!(metrics.closed_time.get(), 0.0);

        failfast
            .entered()
            .now_or_never()
            .expect("timeout must return immediately when in failfast");
        assert!(failfast.is_active(), "failfast should be active");
        assert!(gate_rx.is_shut(), "gate should be shut");

        assert_eq!(metrics.open.get(), 0);
        assert_eq!(metrics.close.get(), 1);
        assert_eq!(metrics.opened_time.get(), 0.0);
        assert_ne!(metrics.closed_time.get(), 0.0);

        failfast.set_ready();
        assert!(!failfast.is_active(), "failfast should be inactive");
        assert!(gate_rx.is_open(), "gate should be open");

        assert_eq!(metrics.open.get(), 1);
        assert_eq!(metrics.close.get(), 1);
        assert_ne!(metrics.opened_time.get(), 0.0);
        assert_eq!(metrics.closed_time.get(), 0.0);

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

        assert_eq!(metrics.open.get(), 1);
        assert_eq!(metrics.close.get(), 1);
        assert_ne!(metrics.opened_time.get(), 0.0);
        assert_eq!(metrics.closed_time.get(), 0.0);

        failfast.entered().await;
        assert!(failfast.is_active(), "failfast should be active");
        assert!(gate_rx.is_shut(), "gate should be shut");

        assert_eq!(metrics.open.get(), 1);
        assert_eq!(metrics.close.get(), 2);
        assert_eq!(metrics.opened_time.get(), 0.0);
        assert_ne!(metrics.closed_time.get(), 0.0);
    }
}
