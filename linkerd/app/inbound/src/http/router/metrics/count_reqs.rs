use super::RouteLabels;
use crate::policy::PermitVariant;
use linkerd_app_core::{
    metrics::prom::{
        self,
        encoding::{EncodeLabelSet, LabelSetEncoder},
        EncodeLabelSetMut,
    },
    svc,
};
use linkerd_http_prom::count_reqs::{self, RequestCount};

/// An `N`-typed `NewService<T>` instrumented with request counting metrics.
pub type NewCountRequests<N> = count_reqs::NewCountRequests<ExtractRequestCount, N>;

#[derive(Clone, Debug)]
pub struct RequestCountFamilies {
    grpc: count_reqs::RequestCountFamilies<RequestCountLabels>,
    http: count_reqs::RequestCountFamilies<RequestCountLabels>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct RequestCountLabels {
    route: RouteLabels,
}

#[derive(Clone, Debug)]
pub struct ExtractRequestCount(pub RequestCountFamilies);

// === impl RequestCountFamilies ===

impl RequestCountFamilies {
    /// Registers a new [`RequestCountFamilies`] with the given registry.
    pub fn register(reg: &mut prom::Registry) -> Self {
        let grpc = {
            let reg = reg.sub_registry_with_prefix("grpc");
            count_reqs::RequestCountFamilies::register(reg)
        };

        let http = {
            let reg = reg.sub_registry_with_prefix("http");
            count_reqs::RequestCountFamilies::register(reg)
        };

        Self { grpc, http }
    }

    /// Fetches the proper request counting family, given a permitted target.
    fn family(
        &self,
        variant: PermitVariant,
    ) -> &count_reqs::RequestCountFamilies<RequestCountLabels> {
        let Self { grpc, http } = self;
        match variant {
            PermitVariant::Grpc => grpc,
            PermitVariant::Http => http,
        }
    }
}

// === impl ExtractRequestCount ===

impl<T> svc::ExtractParam<RequestCount, T> for ExtractRequestCount
where
    T: svc::Param<PermitVariant> + svc::Param<RouteLabels>,
{
    fn extract_param(&self, target: &T) -> RequestCount {
        let Self(families) = self;
        let variant = target.param();
        let route = target.param();

        let family = families.family(variant);

        family.metrics(&RequestCountLabels { route })
    }
}

// === impl RequestCountLabels ===

impl EncodeLabelSetMut for RequestCountLabels {
    fn encode_label_set(&self, enc: &mut LabelSetEncoder<'_>) -> std::fmt::Result {
        let Self { route } = self;

        route.encode_label_set(enc)?;

        Ok(())
    }
}

impl EncodeLabelSet for RequestCountLabels {
    fn encode(&self, mut enc: LabelSetEncoder<'_>) -> std::fmt::Result {
        self.encode_label_set(&mut enc)
    }
}
