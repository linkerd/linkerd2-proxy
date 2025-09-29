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
use linkerd_http_prom::body_data::{self, BodyDataMetrics};

/// An `N`-typed `NewService<T>` instrumented with request body metrics.
pub type NewRecordRequestBodyData<N> = body_data::request::NewRecordBodyData<
    N,
    ExtractRequestBodyDataParams,
    ExtractRequestBodyDataMetrics,
>;

#[derive(Clone, Debug)]
pub struct RequestBodyFamilies {
    grpc: body_data::request::RequestBodyFamilies<RequestBodyDataLabels>,
    http: body_data::request::RequestBodyFamilies<RequestBodyDataLabels>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct RequestBodyDataLabels {
    route: RouteLabels,
}

#[derive(Clone, Debug)]
pub struct ExtractRequestBodyDataParams(RequestBodyFamilies);

#[derive(Clone, Debug)]
pub struct ExtractRequestBodyDataMetrics(BodyDataMetrics);

// === impl RequestBodyFamilies ===

impl RequestBodyFamilies {
    /// Registers a new [`RequestBodyDataFamilies`] with the given registry.
    pub fn register(reg: &mut prom::Registry) -> Self {
        let grpc = {
            let reg = reg.sub_registry_with_prefix("grpc");
            body_data::request::RequestBodyFamilies::register(reg)
        };

        let http = {
            let reg = reg.sub_registry_with_prefix("http");
            body_data::request::RequestBodyFamilies::register(reg)
        };

        Self { grpc, http }
    }

    /// Fetches the proper body frame metrics family, given a permitted target.
    fn family(
        &self,
        variant: PermitVariant,
    ) -> &body_data::request::RequestBodyFamilies<RequestBodyDataLabels> {
        let Self { grpc, http } = self;
        match variant {
            PermitVariant::Grpc => grpc,
            PermitVariant::Http => http,
        }
    }
}

// === impl RequestBodyDataLabels ===

impl EncodeLabelSetMut for RequestBodyDataLabels {
    fn encode_label_set(&self, enc: &mut LabelSetEncoder<'_>) -> std::fmt::Result {
        let Self { route } = self;

        route.encode_label_set(enc)?;

        Ok(())
    }
}

impl EncodeLabelSet for RequestBodyDataLabels {
    fn encode(&self, mut enc: LabelSetEncoder<'_>) -> std::fmt::Result {
        self.encode_label_set(&mut enc)
    }
}

// === impl ExtractRequestBodyDataParams ===

impl ExtractRequestBodyDataParams {
    pub fn new(families: RequestBodyFamilies) -> Self {
        Self(families)
    }
}

impl<T> svc::ExtractParam<ExtractRequestBodyDataMetrics, T> for ExtractRequestBodyDataParams
where
    T: svc::Param<PermitVariant> + svc::Param<RouteLabels>,
{
    fn extract_param(&self, target: &T) -> ExtractRequestBodyDataMetrics {
        let Self(families) = self;
        let variant = target.param();
        let route = target.param();

        let family = families.family(variant);
        let metrics = family.metrics(&RequestBodyDataLabels { route });

        ExtractRequestBodyDataMetrics(metrics)
    }
}

// === impl ExtractRequestBodyDataMetrics ===

impl<T> svc::ExtractParam<BodyDataMetrics, T> for ExtractRequestBodyDataMetrics {
    fn extract_param(&self, _: &T) -> BodyDataMetrics {
        let Self(metrics) = self;

        metrics.clone()
    }
}
