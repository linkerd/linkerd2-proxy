pub use linkerd_proxy_transport::*;
pub use linkerd_transport_metrics as metrics;

pub mod labels;

pub type Metrics = metrics::Registry<labels::Key>;
