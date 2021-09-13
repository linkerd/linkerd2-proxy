use super::ErrorKind;
use linkerd_app_core::{
    metrics::{metrics, Counter, FmtMetrics},
    svc::{self, stack::NewMonitor},
    transport::{labels::TargetAddr, OrigDstAddr},
    Error,
};
use parking_lot::Mutex;
use std::{collections::HashMap, sync::Arc};

metrics! {
    inbound_tcp_errors_total: Counter {
        "The total number of inbound TCP connections that could not be processed due to a proxy error."
    }
}

#[derive(Clone, Debug, Default)]
pub struct TcpErrorMetrics(Arc<Mutex<HashMap<(ErrorKind, TargetAddr), Counter>>>);

#[derive(Clone, Debug)]
pub struct MonitorTcpErrorMetrics {
    target_addr: TargetAddr,
    registry: TcpErrorMetrics,
}

// === impl TcpErrorMetrics ===

impl TcpErrorMetrics {
    pub fn to_layer<N>(&self) -> impl svc::layer::Layer<N, Service = NewMonitor<Self, N>> + Clone {
        NewMonitor::layer(self.clone())
    }
}

impl<T> svc::stack::MonitorNewService<T> for TcpErrorMetrics
where
    T: svc::Param<OrigDstAddr>,
{
    type MonitorService = MonitorTcpErrorMetrics;

    fn monitor(&self, target: &T) -> Self::MonitorService {
        let OrigDstAddr(addr) = target.param();
        MonitorTcpErrorMetrics {
            target_addr: TargetAddr(addr),
            registry: self.clone(),
        }
    }
}

impl FmtMetrics for TcpErrorMetrics {
    fn fmt_metrics(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let metrics = self.0.lock();
        if metrics.is_empty() {
            return Ok(());
        }
        inbound_tcp_errors_total.fmt_help(f)?;
        inbound_tcp_errors_total.fmt_scopes(f, metrics.iter(), |c| c)
    }
}

// === impl MonitorTcpErrorMetrics ===

impl<Req> svc::stack::MonitorService<Req> for MonitorTcpErrorMetrics {
    type MonitorResponse = Self;

    #[inline]
    fn monitor_request(&mut self, _: &Req) -> Self::MonitorResponse {
        self.clone()
    }
}

impl svc::stack::MonitorError<Error> for MonitorTcpErrorMetrics {
    #[inline]
    fn monitor_error(&mut self, e: &Error) {
        if let Some(error) = ErrorKind::mk(&**e) {
            self.registry
                .0
                .lock()
                .entry((error, self.target_addr))
                .or_default()
                .incr();
        }
    }
}
