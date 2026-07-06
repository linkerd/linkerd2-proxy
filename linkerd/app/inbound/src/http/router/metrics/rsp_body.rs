use super::RouteLabels;
use crate::policy::PermitVariant;
use linkerd_app_core::{
    metrics::prom::{
        self,
        encoding::{EncodeLabelSet, LabelSetEncoder},
    },
    svc,
};
use linkerd_http_prom::body_data::{self, BodyDataMetrics};

/// An `N`-typed `NewService<T>` instrumented with response body metrics.
pub type NewRecordResponseBodyData<N> =
    body_data::response::NewRecordBodyData<ExtractResponseBodyDataMetrics, N>;

#[derive(Clone, Debug)]
pub struct ResponseBodyFamilies {
    grpc: body_data::response::ResponseBodyFamilies<ResponseBodyDataLabels>,
    http: body_data::response::ResponseBodyFamilies<ResponseBodyDataLabels>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct ResponseBodyDataLabels {
    route: RouteLabels,
}

#[derive(Clone, Debug)]
pub struct ExtractResponseBodyDataMetrics(ResponseBodyFamilies);

// === impl ResponseBodyFamilies ===

impl ResponseBodyFamilies {
    /// Registers a new [`ResponseBodyDataFamilies`] with the given registry.
    pub fn register(reg: &mut prom::Registry) -> Self {
        let grpc = {
            let reg = reg.sub_registry_with_prefix("grpc");
            body_data::response::ResponseBodyFamilies::register(reg)
        };

        let http = {
            let reg = reg.sub_registry_with_prefix("http");
            body_data::response::ResponseBodyFamilies::register(reg)
        };

        Self { grpc, http }
    }

    /// Fetches the proper body frame metrics family, given a permitted target.
    fn family(
        &self,
        variant: PermitVariant,
    ) -> &body_data::response::ResponseBodyFamilies<ResponseBodyDataLabels> {
        let Self { grpc, http } = self;
        match variant {
            PermitVariant::Grpc => grpc,
            PermitVariant::Http => http,
        }
    }
}

// === impl ResponseBodyDataLabels ===

impl EncodeLabelSet for ResponseBodyDataLabels {
    fn encode(&self, enc: &mut LabelSetEncoder<'_>) -> std::fmt::Result {
        let Self { route } = self;

        route.encode(enc)?;

        Ok(())
    }
}

// === impl ExtractResponseBodyDataMetrics ===

impl ExtractResponseBodyDataMetrics {
    pub fn new(families: ResponseBodyFamilies) -> Self {
        Self(families)
    }
}

impl<T> svc::ExtractParam<BodyDataMetrics, T> for ExtractResponseBodyDataMetrics
where
    T: svc::Param<PermitVariant> + svc::Param<RouteLabels>,
{
    fn extract_param(&self, target: &T) -> BodyDataMetrics {
        let Self(families) = self;
        let variant = target.param();
        let route = target.param();

        let family = families.family(variant);

        family.metrics(&ResponseBodyDataLabels { route })
    }
}
