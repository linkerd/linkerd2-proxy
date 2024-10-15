use linkerd_metrics::prom;
use linkerd_stack::{ExtractParam, Param};
use std::{fmt::Debug, hash::Hash};

pub use self::sensor::ZoneSensorIo;

pub mod client;
mod sensor;

#[derive(Clone, Debug)]
pub struct TcpZoneMetrics {
    pub recv_bytes: prom::Counter,
    pub send_bytes: prom::Counter,
}

#[derive(Clone, Debug)]
pub struct TcpZoneMetricsParams<L> {
    transfer_cost: prom::Family<L, prom::Counter>,
}

impl<L: Clone + Eq + Hash> Default for TcpZoneMetricsParams<L> {
    fn default() -> Self {
        Self {
            transfer_cost: prom::Family::default(),
        }
    }
}

#[derive(Debug, Copy, Clone, Hash, Eq, PartialEq)]
pub struct TcpZoneLabels<L> {
    pub recv_labels: L,
    pub send_labels: L,
}

impl<L> TcpZoneMetricsParams<L>
where
    L: Clone + Hash + Eq + prom::encoding::EncodeLabelSet + Debug + Send + Sync + 'static,
{
    pub fn register(registry: &mut prom::Registry) -> Self {
        let transfer_cost = prom::Family::default();
        registry.register_with_unit(
            "transfer_cost",
            "The total amount of data transferred in and out of this proxy, by cost zone",
            prom::Unit::Bytes,
            transfer_cost.clone(),
        );

        TcpZoneMetricsParams { transfer_cost }
    }
}

impl<L> TcpZoneMetricsParams<L>
where
    L: Clone + Hash + Eq,
{
    pub fn metrics(&self, labels: TcpZoneLabels<L>) -> TcpZoneMetrics {
        let recv_bytes = self
            .transfer_cost
            .get_or_create(&labels.recv_labels)
            .clone();
        let send_bytes = self
            .transfer_cost
            .get_or_create(&labels.send_labels)
            .clone();
        TcpZoneMetrics {
            recv_bytes,
            send_bytes,
        }
    }
}

impl<T, L> ExtractParam<TcpZoneMetrics, T> for TcpZoneMetricsParams<L>
where
    T: Param<TcpZoneLabels<L>>,
    L: Clone + Hash + Eq,
{
    fn extract_param(&self, t: &T) -> TcpZoneMetrics {
        self.metrics(t.param())
    }
}
