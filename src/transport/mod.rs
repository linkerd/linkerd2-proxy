pub use linkerd2_proxy_transport::*;

pub mod labels;

pub type MetricsRegistry = metrics::Registry<labels::Key>;
