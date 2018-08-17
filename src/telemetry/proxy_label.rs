use std::fmt;

use ctx;
use super::metrics::FmtLabels;

pub struct ProxyLabel(pub ctx::Proxy);

// TODO https://github.com/linkerd/linkerd2/issues/1486
impl FmtLabels for ProxyLabel {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.0 {
            ctx::Proxy::Inbound => f.pad("direction=\"inbound\""),
            ctx::Proxy::Outbound => f.pad("direction=\"outbound\""),
        }
    }
}
