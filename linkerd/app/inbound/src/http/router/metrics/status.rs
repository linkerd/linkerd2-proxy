use super::RouteLabels;
use crate::policy::PermitVariant;
use http::StatusCode;
use linkerd_app_core::{
    metrics::prom::{
        self,
        encoding::{EncodeLabel, EncodeLabelSet, LabelSetEncoder},
        EncodeLabelSetMut,
    },
    svc, Error,
};
use linkerd_http_prom::{
    status,
    stream_label::{
        status::{LabelGrpcStatus, LabelHttpStatus, MkLabelGrpcStatus, MkLabelHttpStatus},
        MkStreamLabel, StreamLabel,
    },
};

pub type NewRecordStatusCode<N> =
    status::NewRecordStatusCode<N, ExtractStatusCodeParams, MkLabelStatus, StatusCodeLabels>;

type StatusMetrics = status::StatusMetrics<StatusCodeLabels>;

type Params = status::Params<MkLabelStatus, StatusCodeLabels>;

#[derive(Clone, Debug)]
pub struct StatusCodeFamilies {
    grpc: StatusMetrics,
    http: StatusMetrics,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct StatusCodeLabels {
    /// Labels from the inbound route authorizing traffic.
    route: RouteLabels,
    /// A status code.
    status: Option<u16>,
}

#[derive(Clone, Debug)]
pub struct ExtractStatusCodeParams(pub StatusCodeFamilies);

pub enum MkLabelStatus {
    Grpc {
        mk_label_grpc: MkLabelGrpcStatus,
        route: RouteLabels,
    },
    Http {
        mk_label_http: MkLabelHttpStatus,
        route: RouteLabels,
    },
}

pub enum LabelStatus {
    Grpc {
        label_grpc: LabelGrpcStatus,
        route: RouteLabels,
    },
    Http {
        label_http: LabelHttpStatus,
        route: RouteLabels,
    },
}

// === impl StatusCodeFamilies ===

impl StatusCodeFamilies {
    /// Registers a new [`StatusCodeFamilies`] with the given registry.
    pub fn register(reg: &mut prom::Registry) -> Self {
        let grpc = {
            let reg = reg.sub_registry_with_prefix("grpc");
            status::StatusMetrics::register(reg, "Completed gRPC responses")
        };

        let http = {
            let reg = reg.sub_registry_with_prefix("http");
            status::StatusMetrics::register(reg, "Completed HTTP responses")
        };

        Self { grpc, http }
    }

    /// Fetches the proper status code family, given a permitted target.
    fn family(&self, variant: PermitVariant) -> &StatusMetrics {
        let Self { grpc, http } = self;
        match variant {
            PermitVariant::Grpc => grpc,
            PermitVariant::Http => http,
        }
    }
}

// === impl ExtractStatusCodeParams ===

impl ExtractStatusCodeParams {
    pub fn new(metrics: StatusCodeFamilies) -> Self {
        Self(metrics)
    }
}

impl<T> svc::ExtractParam<Params, T> for ExtractStatusCodeParams
where
    T: svc::Param<PermitVariant> + svc::Param<RouteLabels>,
{
    fn extract_param(&self, target: &T) -> Params {
        let Self(families) = self;
        let route: RouteLabels = target.param();
        let variant: PermitVariant = target.param();

        let metrics = families.family(variant).clone();
        let mk_stream_label = match variant {
            PermitVariant::Grpc => {
                let mk_label_grpc = MkLabelGrpcStatus;
                MkLabelStatus::Grpc {
                    mk_label_grpc,
                    route,
                }
            }
            PermitVariant::Http => {
                let mk_label_http = MkLabelHttpStatus;
                MkLabelStatus::Http {
                    mk_label_http,
                    route,
                }
            }
        };

        Params {
            mk_stream_label,
            metrics,
        }
    }
}

// === impl StatusCodeLabels ===

impl EncodeLabelSetMut for StatusCodeLabels {
    fn encode_label_set(&self, enc: &mut LabelSetEncoder<'_>) -> std::fmt::Result {
        let Self { route, status } = self;

        route.encode_label_set(enc)?;
        ("status", *status).encode(enc.encode_label())?;

        Ok(())
    }
}

impl EncodeLabelSet for StatusCodeLabels {
    fn encode(&self, mut enc: LabelSetEncoder<'_>) -> std::fmt::Result {
        self.encode_label_set(&mut enc)
    }
}

// === impl MkLabelStatus ===

impl MkStreamLabel for MkLabelStatus {
    type StreamLabel = LabelStatus;

    type StatusLabels = StatusCodeLabels;
    type DurationLabels = ();

    fn mk_stream_labeler<B>(&self, req: &http::Request<B>) -> Option<Self::StreamLabel> {
        match self {
            Self::Grpc {
                mk_label_grpc,
                route,
            } => mk_label_grpc
                .mk_stream_labeler(req)
                .map(|label_grpc| LabelStatus::Grpc {
                    label_grpc,
                    route: route.clone(),
                }),
            Self::Http {
                mk_label_http,
                route,
            } => mk_label_http
                .mk_stream_labeler(req)
                .map(|label_http| LabelStatus::Http {
                    label_http,
                    route: route.clone(),
                }),
        }
    }
}

// === impl LabelStatus ===

impl StreamLabel for LabelStatus {
    type StatusLabels = StatusCodeLabels;
    type DurationLabels = ();

    fn init_response<B>(&mut self, rsp: &http::Response<B>) {
        match self {
            Self::Grpc { label_grpc, .. } => label_grpc.init_response(rsp),
            Self::Http { label_http, .. } => label_http.init_response(rsp),
        }
    }

    fn end_response(&mut self, rsp: Result<Option<&http::HeaderMap>, &Error>) {
        match self {
            Self::Grpc { label_grpc, .. } => label_grpc.end_response(rsp),
            Self::Http { label_http, .. } => label_http.end_response(rsp),
        }
    }

    fn status_labels(&self) -> Self::StatusLabels {
        match self {
            Self::Grpc { label_grpc, route } => {
                let route = route.clone();
                let status = label_grpc.status_labels().map(|code| code as u16);
                StatusCodeLabels { route, status }
            }
            Self::Http { label_http, route } => {
                let route = route.clone();
                let status = label_http.status_labels().as_ref().map(StatusCode::as_u16);
                StatusCodeLabels { route, status }
            }
        }
    }

    fn duration_labels(&self) -> Self::DurationLabels {}
}
