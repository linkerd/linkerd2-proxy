use crate::policy::Permitted;
use linkerd_app_core::{
    metrics::{
        prom::{
            self,
            encoding::{self, EncodeLabelSet, LabelSetEncoder},
            EncodeLabelSetMut,
        },
        RouteLabels, ServerLabel,
    },
    svc,
};
use linkerd_http_prom::{
    count_reqs::{NewCountRequests, RequestCount},
    record_response::{MkStreamLabel, StreamLabel},
};

pub(super) fn layer<N>(
    request_count: RequestCountFamilies,
    responses: ResponseMetricFamilies,
) -> impl svc::Layer<N, Service = NewCountRequests<ExtractRequestCount, NewRecordResponse<N>>> {
    svc::layer::mk(move |inner| {
        use svc::Layer as _;

        let count = {
            let extract = ExtractRequestCount(request_count.clone());
            NewCountRequests::layer_via(extract)
        };

        let record = {
            let extract = ExtractResponseMetrics(responses.clone());
            NewRecordResponse::layer_via(extract)
        };

        count.layer(record.layer(inner))
    })
}

#[derive(Clone, Debug)]
pub struct RequestCountFamilies {
    grpc: linkerd_http_prom::count_reqs::RequestCountFamilies<RequestCountLabels>,
    http: linkerd_http_prom::count_reqs::RequestCountFamilies<RequestCountLabels>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct RequestCountLabels {
    route: RouteLabels,
}

#[derive(Clone, Debug)]
pub struct ExtractRequestCount(pub RequestCountFamilies);

pub type NewRecordResponse<N> = linkerd_http_prom::record_response::NewRecordResponse<
    InboundMkStreamLabel,
    ExtractResponseMetrics,
    ResponseMetrics,
    N,
>;

#[derive(Clone, Debug)]
pub struct ResponseMetricFamilies {
    grpc: ResponseMetrics,
    http: ResponseMetrics,
}

pub type ResponseMetrics = linkerd_http_prom::record_response::ResponseMetrics<
    ResponseDurationLabels,
    ResponseStatusLabels,
>;

pub struct InboundMkStreamLabel;

pub struct InboundStreamLabel;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct ResponseDurationLabels {}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct ResponseStatusLabels {}

#[derive(Clone, Debug)]
pub struct ExtractResponseMetrics(pub ResponseMetricFamilies);

// === impl RequestCountFamilies ===

impl RequestCountFamilies {
    /// Registers a new [`RequestCountFamilies`] with the given registry.
    pub fn register(reg: &mut prom::Registry) -> Self {
        let grpc = {
            let reg = reg.sub_registry_with_prefix("grpc");
            linkerd_http_prom::count_reqs::RequestCountFamilies::register(reg)
        };

        let http = {
            let reg = reg.sub_registry_with_prefix("http");
            linkerd_http_prom::count_reqs::RequestCountFamilies::register(reg)
        };

        Self { grpc, http }
    }

    /// Fetches the proper request counting family, given a permitted target.
    fn family<T>(
        &self,
        permitted: &Permitted<T>,
    ) -> &linkerd_http_prom::count_reqs::RequestCountFamilies<RequestCountLabels> {
        let Self { grpc, http } = self;
        match permitted {
            Permitted::Grpc { .. } => grpc,
            Permitted::Http { .. } => http,
        }
    }
}

// === impl ResponseMetricFamilies ===

impl ResponseMetricFamilies {
    /// Registers a new [`ResponseMetricFamilies`] with the given registry.
    pub fn register(reg: &mut prom::Registry) -> Self {
        let grpc = {
            let reg = reg.sub_registry_with_prefix("grpc");
            ResponseMetrics::register(reg, Self::RESPONSE_BUCKETS.iter().copied())
        };

        let http = {
            let reg = reg.sub_registry_with_prefix("http");
            ResponseMetrics::register(reg, Self::RESPONSE_BUCKETS.iter().copied())
        };

        Self { grpc, http }
    }

    // const REQUEST_BUCKETS: &'static [f64] = &[0.05, 0.5, 1.0, 10.0];
    const RESPONSE_BUCKETS: &'static [f64] = &[0.05, 0.5, 1.0, 10.0];

    /// Fetches the proper response metric family, given a permitted target.
    fn family<T>(&self, permitted: &Permitted<T>) -> &ResponseMetrics {
        let Self { grpc, http } = self;
        match permitted {
            Permitted::Grpc { .. } => grpc,
            Permitted::Http { .. } => http,
        }
    }
}

// === impl ExtractRequestCount ===

impl<T> svc::ExtractParam<RequestCount, Permitted<T>> for ExtractRequestCount {
    fn extract_param(&self, permitted: &Permitted<T>) -> RequestCount {
        let Self(families) = self;
        let family = families.family(permitted);
        let route = permitted.route_labels();

        family.metrics(&RequestCountLabels { route })
    }
}

// === impl ExtractResponseMetrics ===

impl<T>
    svc::ExtractParam<
        linkerd_http_prom::record_response::Params<InboundMkStreamLabel, ResponseMetrics>,
        Permitted<T>,
    > for ExtractResponseMetrics
{
    fn extract_param(
        &self,
        permitted: &Permitted<T>,
    ) -> linkerd_http_prom::record_response::Params<InboundMkStreamLabel, ResponseMetrics> {
        let Self(families) = self;
        let metric = families.family(permitted).clone();

        linkerd_http_prom::record_response::Params {
            labeler: InboundMkStreamLabel,
            metric,
        }
    }
}

// === impl InboundMkStreamLabel ===

impl MkStreamLabel for InboundMkStreamLabel {
    type DurationLabels = ResponseDurationLabels;
    type StatusLabels = ResponseStatusLabels;
    type StreamLabel = InboundStreamLabel;

    /// Returns None when the request should not be recorded.
    fn mk_stream_labeler<B>(&self, _: &http::Request<B>) -> Option<Self::StreamLabel> {
        Some(InboundStreamLabel)
    }
}

// === impl InboundStreamLabel ===

impl StreamLabel for InboundStreamLabel {
    type DurationLabels = ResponseDurationLabels;
    type StatusLabels = ResponseStatusLabels;

    fn init_response<B>(&mut self, _rsp: &http::Response<B>) {}

    fn end_response(
        &mut self,
        _trailers: Result<Option<&http::HeaderMap>, &linkerd_app_core::Error>,
    ) {
    }

    fn status_labels(&self) -> Self::StatusLabels {
        Self::StatusLabels {}
    }

    fn duration_labels(&self) -> Self::DurationLabels {
        Self::DurationLabels {}
    }
}

// === impl RequestCountLabels ===

impl EncodeLabelSetMut for RequestCountLabels {
    fn encode_label_set(&self, enc: &mut LabelSetEncoder<'_>) -> std::fmt::Result {
        use encoding::EncodeLabel as _;

        let Self {
            route:
                RouteLabels {
                    server: ServerLabel(parent, port),
                    route,
                },
        } = self;

        ("parent_group", parent.group()).encode(enc.encode_label())?;
        ("parent_kind", parent.kind()).encode(enc.encode_label())?;
        ("parent_name", parent.name()).encode(enc.encode_label())?;
        ("parent_port", *port).encode(enc.encode_label())?;

        ("route_group", route.group()).encode(enc.encode_label())?;
        ("route_kind", route.kind()).encode(enc.encode_label())?;
        ("route_name", route.name()).encode(enc.encode_label())?;

        Ok(())
    }
}

impl EncodeLabelSet for RequestCountLabels {
    fn encode(&self, mut enc: LabelSetEncoder<'_>) -> std::fmt::Result {
        self.encode_label_set(&mut enc)
    }
}

// === impl ResponseDurationLabels ===

impl EncodeLabelSetMut for ResponseDurationLabels {
    fn encode_label_set(&self, _: &mut encoding::LabelSetEncoder<'_>) -> std::fmt::Result {
        Ok(())
    }
}

impl EncodeLabelSet for ResponseDurationLabels {
    fn encode(&self, mut enc: LabelSetEncoder<'_>) -> std::fmt::Result {
        self.encode_label_set(&mut enc)
    }
}

// === impl ResponseStatusLabels ===

impl EncodeLabelSetMut for ResponseStatusLabels {
    fn encode_label_set(&self, _: &mut encoding::LabelSetEncoder<'_>) -> std::fmt::Result {
        Ok(())
    }
}

impl EncodeLabelSet for ResponseStatusLabels {
    fn encode(&self, mut enc: LabelSetEncoder<'_>) -> std::fmt::Result {
        self.encode_label_set(&mut enc)
    }
}
