pub use linkerd_proxy_transport::*;
use linkerd_stack::{ExtractParam, Param};
pub use linkerd_transport_metrics as metrics;
use std::sync::Arc;
use thiserror::Error;

pub mod labels;

#[derive(Clone, Debug)]
pub struct Metrics(metrics::Registry<labels::Key>);

#[derive(Clone, Debug, Error)]
#[error("connection denied on unknown port {0}")]
pub struct DeniedUnknownPort(pub u16);

#[derive(Debug, Error)]
#[error("unauthorized connection from {client_addr} with identity {tls:?} to {dst_addr}")]
pub struct DeniedUnauthorized {
    pub client_addr: Remote<ClientAddr>,
    pub dst_addr: OrigDstAddr,
    pub tls: linkerd_tls::ConditionalServerTls,
}

impl From<metrics::Registry<labels::Key>> for Metrics {
    fn from(reg: metrics::Registry<labels::Key>) -> Self {
        Self(reg)
    }
}

impl<T: Param<labels::Key>> ExtractParam<Arc<metrics::Metrics>, T> for Metrics {
    fn extract_param(&self, t: &T) -> Arc<metrics::Metrics> {
        self.0.metrics(t.param())
    }
}
