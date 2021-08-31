use super::ErrorKind;
use linkerd_app_core::{
    metrics::{metrics, Counter, FmtMetrics},
    svc, Error,
};
use parking_lot::RwLock;
use std::{collections::HashMap, sync::Arc};

metrics! {
    outbound_http_errors_total: Counter {
        "The total number of inbound HTTP requests that could not be processed due to a proxy error."
    }
}

 #[derive(Clone, Debug, Default)]
pub struct Http(Arc<RwLock<HashMap<ErrorKind, Counter>>>);

// === impl Http ===

impl Http {
    pub(crate) fn to_layer<S>(
        &self,
    ) -> impl svc::layer::Layer<S, Service = svc::stack::Monitor<Self, S>> + Clone {
        svc::stack::Monitor::layer(self.clone())
    }
}

impl<Req> svc::stack::MonitorService<Req> for Http {
    type MonitorResponse = Self;

    #[inline]
    fn monitor_request(&mut self, _: &Req) -> Self::MonitorResponse {
        self.clone()
    }
}

impl svc::stack::MonitorError<Error> for Http {
    #[inline]
    fn monitor_error(&mut self, e: &Error) {
        let kind = ErrorKind::mk(&**e);
        self.0.write().entry(kind).or_default().incr();
    }
}

impl FmtMetrics for Http {
    fn fmt_metrics(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let metrics = self.0.read();
        if metrics.is_empty() {
            return Ok(());
        }
        outbound_http_errors_total.fmt_help(f)?;
        outbound_http_errors_total.fmt_scopes(f, metrics.iter(), |c| c)
    }
}
