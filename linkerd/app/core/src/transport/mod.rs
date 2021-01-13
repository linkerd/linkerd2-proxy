pub use linkerd_proxy_transport::*;

pub mod labels;

pub type Metrics = metrics::Registry<labels::Key>;
