use super::RouteLabels;
use crate::policy::PermitVariant;
use linkerd_app_core::{
    metrics::prom::{self, EncodeLabelSetMut},
    svc,
};
use linkerd_http_prom::{
    record_response::{self, Params},
    stream_label::with::MkWithLabels,
};

pub type NewRequestDuration<N> =
    record_response::NewRequestDuration<MkLabelDuration, ExtractRequestDurationMetrics, N>;

pub type RequestDurationParams =
    Params<MkLabelDuration, record_response::RequestMetrics<RequestDurationLabels>>;

#[derive(Clone, Debug)]
pub struct ExtractRequestDurationMetrics(pub RequestDurationFamilies);

#[derive(Clone, Debug)]
pub struct RequestDurationFamilies {
    grpc: record_response::RequestMetrics<RequestDurationLabels>,
    http: record_response::RequestMetrics<RequestDurationLabels>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct RequestDurationLabels {
    route: RouteLabels,
}

pub type MkLabelDuration = MkWithLabels<RequestDurationLabels>;

// === impl RequestDurationFamilies ===

impl RequestDurationFamilies {
    /// Registers a new [`RequestDurationFamilies`] with the given registry.
    pub fn register(
        reg: &mut prom::Registry,
        histo: impl Clone + IntoIterator<Item = f64>,
    ) -> Self {
        let grpc = {
            let reg = reg.sub_registry_with_prefix("grpc");
            record_response::RequestMetrics::register(reg, histo.clone())
        };

        let http = {
            let reg = reg.sub_registry_with_prefix("http");
            record_response::RequestMetrics::register(reg, histo)
        };

        Self { grpc, http }
    }
}

// === impl ExtractRequestDurationMetrics ===

impl<T> svc::ExtractParam<RequestDurationParams, T> for ExtractRequestDurationMetrics
where
    T: svc::Param<PermitVariant> + svc::Param<RouteLabels>,
{
    fn extract_param(&self, target: &T) -> RequestDurationParams {
        let Self(families) = self;

        let labeler = {
            let route: RouteLabels = target.param();
            let labels = RequestDurationLabels { route };
            MkLabelDuration::new(labels)
        };

        let metric = {
            let variant: PermitVariant = target.param();
            let RequestDurationFamilies { grpc, http } = families;
            match variant {
                PermitVariant::Grpc => grpc,
                PermitVariant::Http => http,
            }
            .clone()
        };

        RequestDurationParams { labeler, metric }
    }
}

// === impl RequestDurationLabels ===

impl prom::EncodeLabelSetMut for RequestDurationLabels {
    fn encode_label_set(
        &self,
        encoder: &mut prom::encoding::LabelSetEncoder<'_>,
    ) -> std::fmt::Result {
        let Self { route } = self;
        route.encode_label_set(encoder)?;
        Ok(())
    }
}

impl prom::encoding::EncodeLabelSet for RequestDurationLabels {
    fn encode(&self, mut encoder: prom::encoding::LabelSetEncoder<'_>) -> std::fmt::Result {
        self.encode_label_set(&mut encoder)
    }
}
