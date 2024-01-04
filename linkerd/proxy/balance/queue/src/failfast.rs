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
    open: prom::Family<L, prom::Counter>,
    shut: prom::Family<L, prom::Counter>,
    open_time: prom::Family<L, prom::Gauge<f64, AtomicU64>>,
    shut_time: prom::Family<L, prom::Gauge<f64, AtomicU64>>,
    timeout: prom::Family<L, prom::Gauge<f64, AtomicU64>>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct GateMetrics {
    open: prom::Counter,
    shut: prom::Counter,
    open_time: prom::Gauge<f64, AtomicU64>,
    shut_time: prom::Gauge<f64, AtomicU64>,
    timeout: prom::Gauge<f64, AtomicU64>,
}

// === impl Failfast ===

impl Failfast {
    pub(super) fn new(timeout: time::Duration, gate: gate::Tx, metrics: GateMetrics) -> Self {
        metrics.timeout.set(timeout.as_secs_f64());
        metrics.open();

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
            self.metrics.open();
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
        self.metrics.close();
    }
}

// === impl GateMetricFamilies ===

impl<L> Default for GateMetricFamilies<L>
where
    L: prom::encoding::EncodeLabelSet + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone,
{
    fn default() -> Self {
        Self {
            open: prom::Family::default(),
            shut: prom::Family::default(),
            open_time: prom::Family::default(),
            shut_time: prom::Family::default(),
            timeout: prom::Family::default(),
        }
    }
}

impl<L> GateMetricFamilies<L>
where
    L: prom::encoding::EncodeLabelSet + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone + Send + Sync + 'static,
{
    pub(crate) fn register(reg: &mut prom::Registry) -> Self {
        let open = prom::Family::default();
        reg.register(
            "open",
            "The total number of gate state changes to open",
            open.clone(),
        );

        let shut = prom::Family::default();
        reg.register(
            "shut",
            "The total number of gate state changes from open to shut",
            shut.clone(),
        );

        let open_time = prom::Family::default();
        reg.register_with_unit(
            "open_time",
            "The time at which the gate was opened",
            prom::Unit::Seconds,
            open_time.clone(),
        );

        let shut_time = prom::Family::default();
        reg.register_with_unit(
            "shut_time",
            "The time at which the gate was shut",
            prom::Unit::Seconds,
            shut_time.clone(),
        );

        let timeout = prom::Family::default();
        reg.register_with_unit(
            "timeout",
            "Indicates the request timeout after which the gate will shut and enter failfast",
            prom::Unit::Seconds,
            timeout.clone(),
        );

        Self {
            open,
            open_time,
            shut,
            shut_time,
            timeout,
        }
    }

    pub(crate) fn metrics(&self, labels: &L) -> GateMetrics {
        let open = self.open.get_or_create(labels).clone();
        let shut = self.shut.get_or_create(labels).clone();
        let open_time = self.open_time.get_or_create(labels).clone();
        let shut_time = self.shut_time.get_or_create(labels).clone();
        let timeout = self.timeout.get_or_create(labels).clone();
        GateMetrics {
            open,
            shut,
            open_time,
            shut_time,
            timeout,
        }
    }
}

impl GateMetrics {
    fn open(&self) {
        self.open.inc();
        self.open_time.set(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_else(|_| Default::default())
                .as_secs_f64(),
        );
        self.shut_time.set(0.0);
    }

    fn close(&self) {
        self.shut.inc();
        self.shut_time.set(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_else(|_| Default::default())
                .as_secs_f64(),
        );
        self.open_time.set(0.0);
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

        assert_eq!(metrics.open.get(), 1);
        assert_eq!(metrics.shut.get(), 0);
        assert_ne!(metrics.open_time.get(), 0.0);
        assert_eq!(metrics.shut_time.get(), 0.0);

        failfast.entered().await;
        assert!(failfast.is_active(), "failfast should be active");
        assert!(gate_rx.is_shut(), "gate should be shut");

        assert_eq!(metrics.open.get(), 1);
        assert_eq!(metrics.shut.get(), 1);
        assert_eq!(metrics.open_time.get(), 0.0);
        assert_ne!(metrics.shut_time.get(), 0.0);

        failfast
            .entered()
            .now_or_never()
            .expect("timeout must return immediately when in failfast");
        assert!(failfast.is_active(), "failfast should be active");
        assert!(gate_rx.is_shut(), "gate should be shut");

        assert_eq!(metrics.open.get(), 1);
        assert_eq!(metrics.shut.get(), 1);
        assert_eq!(metrics.open_time.get(), 0.0);
        assert_ne!(metrics.shut_time.get(), 0.0);

        failfast.set_ready();
        assert!(!failfast.is_active(), "failfast should be inactive");
        assert!(gate_rx.is_open(), "gate should be open");

        assert_eq!(metrics.open.get(), 2);
        assert_eq!(metrics.shut.get(), 1);
        assert_ne!(metrics.open_time.get(), 0.0);
        assert_eq!(metrics.shut_time.get(), 0.0);

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

        assert_eq!(metrics.open.get(), 2);
        assert_eq!(metrics.shut.get(), 1);
        assert_ne!(metrics.open_time.get(), 0.0);
        assert_eq!(metrics.shut_time.get(), 0.0);

        failfast.entered().await;
        assert!(failfast.is_active(), "failfast should be active");
        assert!(gate_rx.is_shut(), "gate should be shut");

        assert_eq!(metrics.open.get(), 2);
        assert_eq!(metrics.shut.get(), 2);
        assert_eq!(metrics.open_time.get(), 0.0);
        assert_ne!(metrics.shut_time.get(), 0.0);
    }
}
