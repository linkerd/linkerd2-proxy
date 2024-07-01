use super::super::metrics::{RouteBackendLabels, RouteLabels};
use crate::{BackendRef, ParentRef, RouteRef};
use linkerd_app_core::{metrics::prom, svc};

#[derive(Clone, Debug, Default)]
pub struct RouteBackendMetrics {
    metrics: super::count_reqs::RequestCountFamilies<RouteBackendLabels>,
}

// === impl RouteBackendMetrics ===

impl RouteBackendMetrics {
    pub fn register(reg: &mut prom::Registry) -> Self {
        Self {
            metrics: super::count_reqs::RequestCountFamilies::register(reg),
        }
    }

    #[cfg(test)]
    pub(crate) fn request_count(
        &self,
        p: ParentRef,
        r: RouteRef,
        b: BackendRef,
    ) -> super::count_reqs::RequestCount {
        self.metrics
            .metrics(&RouteBackendLabels(RouteLabels(p, r), b))
    }
}

impl<T> svc::ExtractParam<super::count_reqs::RequestCount, T> for RouteBackendMetrics
where
    T: svc::Param<ParentRef> + svc::Param<RouteRef> + svc::Param<BackendRef>,
{
    fn extract_param(&self, t: &T) -> super::count_reqs::RequestCount {
        self.metrics.metrics(&RouteBackendLabels(
            RouteLabels(t.param(), t.param()),
            t.param(),
        ))
    }
}
