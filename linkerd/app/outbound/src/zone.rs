use linkerd_app_core::{metrics::OutboundZoneLocality, transport::metrics::zone};
use prometheus_client::encoding::{EncodeLabelSet, EncodeLabelValue};
use std::{fmt::Debug, hash::Hash};

pub type TcpZoneMetrics = zone::TcpZoneMetricsParams<TcpZoneOpLabel>;
pub type TcpZoneLabels = zone::TcpZoneLabels<TcpZoneOpLabel>;

#[derive(Debug, Copy, Clone, Hash, Eq, PartialEq, EncodeLabelSet)]
pub struct TcpZoneOpLabel {
    op: TcpZoneOp,
    zone_locality: OutboundZoneLocality,
}

impl TcpZoneOpLabel {
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

pub fn tcp_zone_labels(zone_locality: OutboundZoneLocality) -> TcpZoneLabels {
    zone::TcpZoneLabels {
        recv_labels: TcpZoneOpLabel::new_recv_labels(zone_locality),
        send_labels: TcpZoneOpLabel::new_send_labels(zone_locality),
    }
}

#[derive(Debug, Copy, Clone, Hash, Eq, PartialEq, EncodeLabelValue)]
enum TcpZoneOp {
    Send,
    Recv,
}
