use linkerd_errno::Errno;
use linkerd_metrics::prom;
use prom::{encoding::*, EncodeLabelSetMut};
use std::{fmt::Debug, hash::Hash};

pub mod client;

#[derive(Clone, Debug)]
pub struct TcpMetricsParams<L: Clone> {
    connections_opened: prom::Family<L, prom::Counter>,
    connections_closed: prom::Family<ConnectionsClosedLabels<L>, prom::Counter>,
}

#[derive(Clone, Debug)]
pub struct TcpMetrics {
    pub connections_opened: prom::Counter,
    pub connections_closed: prom::Counter,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct ConnectionsClosedLabels<L> {
    labels: L,
    error: Option<Errno>,
}

// === impl TcpMetricsParams ===

impl<L> Default for TcpMetricsParams<L>
where
    L: Clone + Eq + Hash,
{
    fn default() -> Self {
        Self {
            connections_opened: prom::Family::default(),
            connections_closed: prom::Family::default(),
        }
    }
}

impl<L> TcpMetricsParams<L>
where
    L: Clone + Hash + Eq + EncodeLabelSetMut + Debug + Send + Sync + 'static,
{
    pub fn register(registry: &mut prom::Registry) -> Self {
        let connections_opened = prom::Family::default();
        registry.register(
            "open",
            "The number of connections opened",
            connections_opened.clone(),
        );

        let connections_closed = prom::Family::default();
        registry.register(
            "close",
            "The number of connections closed",
            connections_closed.clone(),
        );

        TcpMetricsParams {
            connections_opened,
            connections_closed,
        }
    }
}

impl<L> TcpMetricsParams<L>
where
    L: Clone + Hash + Eq,
{
    pub fn metrics_open(&self, labels: L) -> prom::Counter {
        self.connections_opened.get_or_create(&labels).clone()
    }

    pub fn metrics_closed(&self, labels: L, error: Option<Errno>) -> prom::Counter {
        let labels = ConnectionsClosedLabels { labels, error };
        self.connections_closed.get_or_create(&labels).clone()
    }
}

// === impl ConnectionsClosedLabels ===

impl<L> EncodeLabelSetMut for ConnectionsClosedLabels<L>
where
    L: Clone + Hash + Eq + EncodeLabelSetMut + Debug + Send + Sync + 'static,
{
    fn encode_label_set(&self, enc: &mut LabelSetEncoder<'_>) -> std::fmt::Result {
        self.labels.encode_label_set(enc)?;
        match self.error {
            Some(error) => ("error", error.to_string()).encode(enc.encode_label())?,
            None => ("error", "").encode(enc.encode_label())?,
        }

        Ok(())
    }
}

impl<L> EncodeLabelSet for ConnectionsClosedLabels<L>
where
    L: Clone + Hash + Eq + EncodeLabelSetMut + Debug + Send + Sync + 'static,
{
    fn encode(&self, mut enc: LabelSetEncoder<'_>) -> std::fmt::Result {
        self.encode_label_set(&mut enc)
    }
}
