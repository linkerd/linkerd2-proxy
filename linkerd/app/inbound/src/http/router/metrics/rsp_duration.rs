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

pub type NewResponseDuration<N> =
    record_response::NewResponseDuration<MkLabelDuration, ExtractResponseDurationMetrics, N>;

pub type ResponseDurationParams =
    Params<MkLabelDuration, record_response::ResponseMetrics<ResponseDurationLabels>>;

#[derive(Clone, Debug)]
pub struct ExtractResponseDurationMetrics(pub ResponseDurationFamilies);

#[derive(Clone, Debug)]
pub struct ResponseDurationFamilies {
    grpc: record_response::ResponseMetrics<ResponseDurationLabels>,
    http: record_response::ResponseMetrics<ResponseDurationLabels>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct ResponseDurationLabels {
    route: RouteLabels,
}

pub type MkLabelDuration = MkWithLabels<ResponseDurationLabels>;

// === impl ResponseDurationFamilies ===

impl ResponseDurationFamilies {
    /// Registers a new [`ResponseDurationFamilies`] with the given registry.
    pub fn register(
        reg: &mut prom::Registry,
        histo: impl Clone + IntoIterator<Item = f64>,
    ) -> Self {
        let grpc = {
            let reg = reg.sub_registry_with_prefix("grpc");
            record_response::ResponseMetrics::register(reg, histo.clone())
        };

        let http = {
            let reg = reg.sub_registry_with_prefix("http");
            record_response::ResponseMetrics::register(reg, histo)
        };

        Self { grpc, http }
    }
}

// === impl ExtractResponseDurationMetrics ===

impl<T> svc::ExtractParam<ResponseDurationParams, T> for ExtractResponseDurationMetrics
where
    T: svc::Param<PermitVariant> + svc::Param<RouteLabels>,
{
    fn extract_param(&self, target: &T) -> ResponseDurationParams {
        let Self(families) = self;

        let labeler = {
            let route: RouteLabels = target.param();
            let labels = ResponseDurationLabels { route };
            MkLabelDuration::new(labels)
        };

        let metric = {
            let variant: PermitVariant = target.param();
            let ResponseDurationFamilies { grpc, http } = families;
            match variant {
                PermitVariant::Grpc => grpc,
                PermitVariant::Http => http,
            }
            .clone()
        };

        ResponseDurationParams { labeler, metric }
    }
}

// === impl ResponseDurationLabels ===

impl prom::EncodeLabelSetMut for ResponseDurationLabels {
    fn encode_label_set(
        &self,
        encoder: &mut prom::encoding::LabelSetEncoder<'_>,
    ) -> std::fmt::Result {
        let Self { route } = self;
        route.encode_label_set(encoder)?;
        Ok(())
    }
}

impl prom::encoding::EncodeLabelSet for ResponseDurationLabels {
    fn encode(&self, mut encoder: prom::encoding::LabelSetEncoder<'_>) -> std::fmt::Result {
        self.encode_label_set(&mut encoder)
    }
}
