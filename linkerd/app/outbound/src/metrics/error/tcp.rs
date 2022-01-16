use super::ErrorKind;
use ahash::AHashMap as HashMap;
use linkerd_app_core::{
    metrics::{metrics, Counter, FmtMetrics},
    svc,
    transport::{labels::TargetAddr, OrigDstAddr},
    Error,
};
use parking_lot::RwLock;
use std::sync::Arc;

metrics! {
    outbound_tcp_errors_total: Counter {
        "The total number of outbound TCP connections that could not be processed due to a proxy error."
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Tcp(Arc<RwLock<HashMap<(TargetAddr, ErrorKind), Counter>>>);

#[derive(Clone, Debug)]
pub(crate) struct MonitorTcp {
    target_addr: TargetAddr,
    registry: Tcp,
}

// === impl Tcp ===

impl Tcp {
    pub(crate) fn to_layer<N>(
        &self,
    ) -> impl svc::layer::Layer<N, Service = svc::stack::NewMonitor<Self, N>> + Clone {
        svc::stack::NewMonitor::layer(self.clone())
    }
}

impl<T: svc::Param<OrigDstAddr>> svc::stack::MonitorNewService<T> for Tcp {
    type MonitorService = MonitorTcp;

    fn monitor(&self, target: &T) -> Self::MonitorService {
        let OrigDstAddr(addr) = target.param();
        MonitorTcp {
            target_addr: TargetAddr(addr),
            registry: self.clone(),
        }
    }
}

impl FmtMetrics for Tcp {
    fn fmt_metrics(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let metrics = self.0.read();
        if metrics.is_empty() {
            return Ok(());
        }
        outbound_tcp_errors_total.fmt_help(f)?;
        outbound_tcp_errors_total.fmt_scopes(f, metrics.iter(), |c| c)
    }
}

// === impl MonitorTcp ===

impl<Req> svc::stack::MonitorService<Req> for MonitorTcp {
    type MonitorResponse = Self;

    #[inline]
    fn monitor_request(&mut self, _: &Req) -> Self::MonitorResponse {
        self.clone()
    }
}

impl svc::stack::MonitorError<Error> for MonitorTcp {
    #[inline]
    fn monitor_error(&mut self, e: &Error) {
        let kind = ErrorKind::mk(&**e);
        self.registry
            .0
            .write()
            .entry((self.target_addr, kind))
            .or_default()
            .incr();
    }
}
