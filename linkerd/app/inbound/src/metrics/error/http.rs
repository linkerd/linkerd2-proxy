use super::ErrorKind;
use linkerd_app_core::{
    metrics::{metrics, Counter, FmtMetrics, ServerLabel},
    svc::{self, stack::NewMonitor},
    transport::{labels::TargetAddr, OrigDstAddr},
    Error,
};
use parking_lot::Mutex;
use std::{collections::HashMap, sync::Arc};

metrics! {
    inbound_http_errors_total: Counter {
        "The total number of inbound HTTP requests that could not be processed due to a proxy error."
    }
}

#[derive(Clone, Debug, Default)]
pub struct HttpErrorMetrics(Arc<Mutex<HashMap<(ErrorKind, (TargetAddr, ServerLabel)), Counter>>>);

#[derive(Clone, Debug)]
pub struct MonitorHttpErrorMetrics {
    labels: (TargetAddr, ServerLabel),
    registry: HttpErrorMetrics,
}

// === impl HttpErrorMetrics ===

impl HttpErrorMetrics {
    pub fn to_layer<S>(&self) -> impl svc::layer::Layer<S, Service = NewMonitor<Self, S>> + Clone {
        NewMonitor::layer(self.clone())
    }
}

impl<T> svc::stack::MonitorNewService<T> for HttpErrorMetrics
where
    T: svc::Param<OrigDstAddr>,
    T: svc::Param<ServerLabel>,
{
    type MonitorService = MonitorHttpErrorMetrics;

    #[inline]
    fn monitor(&self, target: &T) -> Self::MonitorService {
        let OrigDstAddr(addr) = target.param();
        MonitorHttpErrorMetrics {
            labels: (TargetAddr(addr), target.param()),
            registry: self.clone(),
        }
    }
}

impl FmtMetrics for HttpErrorMetrics {
    fn fmt_metrics(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let metrics = self.0.lock();
        if metrics.is_empty() {
            return Ok(());
        }
        inbound_http_errors_total.fmt_help(f)?;
        inbound_http_errors_total.fmt_scopes(f, metrics.iter(), |c| c)
    }
}

// === impl MonitorHttpErrorMetrics ===

impl<Req> svc::stack::MonitorService<Req> for MonitorHttpErrorMetrics {
    type MonitorResponse = Self;

    #[inline]
    fn monitor_request(&mut self, _: &Req) -> Self::MonitorResponse {
        self.clone()
    }
}

impl svc::stack::MonitorError<Error> for MonitorHttpErrorMetrics {
    #[inline]
    fn monitor_error(&mut self, e: &Error) {
        if let Some(error) = ErrorKind::mk(&**e) {
            self.registry
                .0
                .lock()
                .entry((error, self.labels.clone()))
                .or_default()
                .incr();
        }
    }
}
