use super::{Eos, EosMetrics, Metrics};
use linkerd_errno::Errno;
use linkerd_io as io;
use std::{sync::Arc, task::Poll};
use tokio::time::Instant;

/// Tracks the state of a single instance of `Io` throughout its lifetime.
#[derive(Debug)]
pub struct Sensor {
    metrics: Option<Arc<Metrics>>,
}

pub type SensorIo<T> = io::SensorIo<T, Sensor>;

// === impl Sensor ===

impl Sensor {
    pub(crate) fn open(metrics: Arc<Metrics>) -> Self {
        metrics.open_total.incr();
        metrics.open_connections.incr();
        metrics.by_eos.lock().last_update = Instant::now();
        Self {
            metrics: Some(metrics),
        }
    }
}

impl io::Sensor for Sensor {
    fn record_read(&mut self, sz: usize) {
        if let Some(ref m) = self.metrics {
            m.read_bytes_total.add(sz as u64);
            m.by_eos.lock().last_update = Instant::now();
        }
    }

    fn record_write(&mut self, sz: usize) {
        if let Some(ref m) = self.metrics {
            m.write_bytes_total.add(sz as u64);
            m.by_eos.lock().last_update = Instant::now();
        }
    }

    fn record_close(&mut self, eos: Option<Errno>) {
        // When closed, the metrics structure is dropped so that no further
        // updates can occur (i.e. so that an additional close won't be recorded
        // on Drop).
        if let Some(m) = self.metrics.take() {
            m.open_connections.decr();

            let mut by_eos = m.by_eos.lock();
            let class = by_eos
                .metrics
                .entry(Eos(eos))
                .or_insert_with(EosMetrics::default);
            class.close_total.incr();
            by_eos.last_update = Instant::now();
        }
    }

    /// Wraps an operation on the underlying transport with error telemetry.
    ///
    /// If the transport operation results in a non-recoverable error, record a
    /// transport closure.
    fn record_error<T>(&mut self, op: Poll<std::io::Result<T>>) -> Poll<std::io::Result<T>> {
        match op {
            Poll::Ready(Ok(v)) => Poll::Ready(Ok(v)),
            Poll::Ready(Err(e)) => {
                if e.kind() != std::io::ErrorKind::WouldBlock {
                    let eos = e.raw_os_error().map(|e| e.into());
                    self.record_close(eos);
                }

                Poll::Ready(Err(e))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for Sensor {
    fn drop(&mut self) {
        io::Sensor::record_close(self, None)
    }
}
