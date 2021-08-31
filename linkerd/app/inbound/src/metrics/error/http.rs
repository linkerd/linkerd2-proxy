use super::ErrorKind;
use linkerd_app_core::{
    metrics::{metrics, Counter, FmtMetrics},
    svc,
    transport::{labels::TargetAddr, OrigDstAddr},
    Error,
};
use parking_lot::RwLock;
use std::{collections::HashMap, sync::Arc};

metrics! {
    inbound_http_errors_total: Counter {
        "The total number of inbound HTTP requests that could not be processed due to a proxy error."
    }
}

#[derive(Clone, Debug, Default)]
pub struct Http(Arc<RwLock<HashMap<(TargetAddr, ErrorKind), Counter>>>);

#[derive(Clone, Debug)]
pub struct MonitorHttp {
    target_addr: TargetAddr,
    registry: Http,
}

// === impl Http ===

impl Http {
    pub fn to_layer<S>(
        &self,
    ) -> impl svc::layer::Layer<S, Service = svc::stack::NewMonitor<Self, S>> + Clone {
        svc::stack::NewMonitor::layer(self.clone())
    }
}

impl<T: svc::Param<OrigDstAddr>> svc::stack::MonitorNewService<T> for Http {
    type MonitorService = MonitorHttp;

    #[inline]
    fn monitor(&mut self, target: &T) -> Self::MonitorService {
        let OrigDstAddr(addr) = target.param();
        MonitorHttp {
            target_addr: TargetAddr(addr),
            registry: self.clone(),
        }
    }
}

impl FmtMetrics for Http {
    fn fmt_metrics(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let metrics = self.0.read();
        if metrics.is_empty() {
            return Ok(());
        }
        inbound_http_errors_total.fmt_help(f)?;
        inbound_http_errors_total.fmt_scopes(f, metrics.iter(), |c| c)
    }
}

// === impl MonitorHttp ===

impl<Req> svc::stack::MonitorService<Req> for MonitorHttp {
    type MonitorResponse = Self;

    #[inline]
    fn monitor_request(&mut self, _: &Req) -> Self::MonitorResponse {
        self.clone()
    }
}

impl svc::stack::MonitorError<Error> for MonitorHttp {
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
