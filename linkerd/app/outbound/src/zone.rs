use crate::metrics::prom;
use linkerd_app_core::{
    metrics::OutboundZoneLocality, svc, transport::metrics::zone::TcpZoneMetrics,
};
use prometheus_client::{
    encoding::{EncodeLabelSet, EncodeLabelValue},
    registry::Unit,
};

#[derive(Clone, Debug, Default)]
pub struct TcpZoneMetricsParams {
    transfer_cost: prom::Family<TcpZoneLabels, prom::Counter>,
}

impl TcpZoneMetricsParams {
    pub fn register(registry: &mut prom::Registry) -> Self {
        let transfer_cost = prom::Family::default();
        registry.register_with_unit(
            "transfer_cost",
            "The total cost of data transferred in and out of this proxy in bytes",
            Unit::Bytes,
            transfer_cost.clone(),
        );

        TcpZoneMetricsParams { transfer_cost }
    }

    pub fn metrics(&self, zone_locality: OutboundZoneLocality) -> TcpZoneMetrics {
        let recv_bytes = self
            .transfer_cost
            .get_or_create(&TcpZoneLabels::new_recv_labels(zone_locality))
            .clone();
        let send_bytes = self
            .transfer_cost
            .get_or_create(&TcpZoneLabels::new_send_labels(zone_locality))
            .clone();
        TcpZoneMetrics {
            recv_bytes,
            send_bytes,
        }
    }
}

#[derive(Debug, Copy, Clone, Hash, Eq, PartialEq, EncodeLabelSet)]
pub struct TcpZoneLabels {
    op: TcpZoneOp,
    zone_locality: OutboundZoneLocality,
}

impl TcpZoneLabels {
    fn new_send_labels(zone_locality: OutboundZoneLocality) -> Self {
        Self {
            op: TcpZoneOp::Send,
            zone_locality,
        }
    }

    fn new_recv_labels(zone_locality: OutboundZoneLocality) -> Self {
        Self {
            op: TcpZoneOp::Recv,
            zone_locality,
        }
    }
}

#[derive(Debug, Copy, Clone, Hash, Eq, PartialEq, EncodeLabelValue)]
enum TcpZoneOp {
    Send,
    Recv,
}

impl<T> svc::ExtractParam<TcpZoneMetrics, T> for TcpZoneMetricsParams
where
    T: svc::Param<OutboundZoneLocality>,
{
    fn extract_param(&self, target: &T) -> TcpZoneMetrics {
        self.metrics(target.param())
    }
}
