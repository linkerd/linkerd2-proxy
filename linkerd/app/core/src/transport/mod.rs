pub use linkerd_proxy_transport::*;
use linkerd_stack::{ExtractParam, Param};
pub use linkerd_transport_metrics as metrics;
use std::sync::Arc;

pub mod labels;

#[derive(Clone, Debug)]
pub struct Metrics(metrics::Registry<labels::Key>);

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
