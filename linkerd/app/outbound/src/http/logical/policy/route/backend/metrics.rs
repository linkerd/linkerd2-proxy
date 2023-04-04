use ahash::AHashMap;
use linkerd_app_core::metrics::{metrics, Counter, FmtLabels, FmtMetrics};
use linkerd_proxy_client_policy as policy;
use parking_lot::Mutex;
use std::{fmt::Write, sync::Arc};

use crate::{BackendRef, ParentRef, RouteRef};

metrics! {
    outbound_http_route_backend_requests_total: Counter {
        "The total number of outbound requests dispatched to a HTTP route backend"
    },
    outbound_grpc_route_backend_requests_total: Counter {
        "The total number of outbound requests dispatched to a gRPC route backend"
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Labels(ParentRef, RouteRef, BackendRef);

#[derive(Clone, Debug, Default)]
pub struct RouteBackendMetrics {
    http: Arc<Mutex<AHashMap<Labels, Arc<Counter>>>>,
    grpc: Arc<Mutex<AHashMap<Labels, Arc<Counter>>>>,
}

// === impl RouteBackendMetrics ===

impl RouteBackendMetrics {
    pub fn http_requests_total(&self, pr: ParentRef, rr: RouteRef, br: BackendRef) -> Arc<Counter> {
        self.http
            .lock()
            .entry(Labels(pr, rr, br))
            .or_default()
            .clone()
    }

    pub fn grpc_requests_total(&self, pr: ParentRef, rr: RouteRef, br: BackendRef) -> Arc<Counter> {
        self.grpc
            .lock()
            .entry(Labels(pr, rr, br))
            .or_default()
            .clone()
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

impl FmtLabels for Labels {
    fn fmt_labels(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self(parent, route, backend) = self;

        Self::write_meta("parent", parent, f)?;
        f.write_char(',')?;

        Self::write_meta("route", route, f)?;
        f.write_char(',')?;

        Self::write_meta("backend", backend, f)?;
        Ok(())
    }
}

impl Labels {
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
