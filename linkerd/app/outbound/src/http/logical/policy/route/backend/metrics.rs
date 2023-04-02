use crate::{BackendRef, ParentRef};
use ahash::AHashMap;
use linkerd_app_core::metrics::{metrics, Counter, FmtLabels, FmtMetrics};
use linkerd_proxy_client_policy as policy;
use parking_lot::Mutex;
use std::{fmt::Write, sync::Arc};

metrics! {
    outbound_http_route_backend_requests_total: Counter {
        "The total number of outbound requests dispatched to a HTTP route backend"
    },
    outbound_grpc_route_backend_requests_total: Counter {
        "The total number of outbound requests dispatched to a gRPC route backend"
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct RouteBackendMeta {
    pub parent: ParentRef,
    pub route: Arc<policy::Meta>,
    pub backend: BackendRef,
}

#[derive(Clone, Debug, Default)]
pub struct RouteBackendMetrics {
    http: Arc<Mutex<AHashMap<RouteBackendMeta, Arc<Counter>>>>,
    grpc: Arc<Mutex<AHashMap<RouteBackendMeta, Arc<Counter>>>>,
}

// === impl RouteBackendMetrics ===

impl RouteBackendMetrics {
    pub fn http_requests_total(&self, meta: RouteBackendMeta) -> Arc<Counter> {
        self.http.lock().entry(meta).or_default().clone()
    }

    pub fn grpc_requests_total(&self, meta: RouteBackendMeta) -> Arc<Counter> {
        self.grpc.lock().entry(meta).or_default().clone()
    }
}

impl FmtMetrics for RouteBackendMetrics {
    fn fmt_metrics(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let http = self.http.lock();
        if !http.is_empty() {
            outbound_http_route_backend_requests_total.fmt_help(f)?;
            outbound_http_route_backend_requests_total.fmt_scopes(f, http.iter(), |c| c)?;
        }
        drop(http);

        let grpc = self.grpc.lock();
        if !grpc.is_empty() {
            outbound_grpc_route_backend_requests_total.fmt_help(f)?;
            outbound_grpc_route_backend_requests_total.fmt_scopes(f, grpc.iter(), |c| c)?;
        }
        drop(grpc);

        Ok(())
    }
}

impl FmtLabels for RouteBackendMeta {
    fn fmt_labels(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Self::write_meta("parent", &self.parent.0, f)?;
        f.write_char(',')?;

        Self::write_meta("route", &self.route, f)?;
        f.write_char(',')?;

        Self::write_meta("backend", &self.backend.0, f)?;
        Ok(())
    }
}

impl RouteBackendMeta {
    fn write_meta(
        scope: &str,
        meta: &policy::Meta,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        write!(f, "{scope}_group=\"{}\"", meta.group())?;
        write!(f, ",{scope}_kind=\"{}\"", meta.kind())?;
        write!(f, ",{scope}_namespace=\"{}\"", meta.namespace())?;
        write!(f, ",{scope}_name=\"{}\"", meta.name())?;
        Ok(())
    }
}
