use super::{backend::metrics as backend, retry};
use linkerd_app_core::{
    metrics::prom::{self, EncodeLabelSetMut},
    svc,
};
use linkerd_http_prom::record_response::{self, ResponseMetrics, StreamLabel};

pub use linkerd_http_prom::record_response::MkStreamLabel;

pub type RequestMetrics<DurL, TotL> = record_response::RequestMetrics<DurL, TotL>;

#[derive(Debug)]
pub struct RouteMetrics<R: StreamLabel, B: StreamLabel> {
    pub(super) retry: retry::RouteRetryMetrics,
    pub(super) requests: RequestMetrics<R::DurationLabels, R::TotalLabels>,
    pub(super) backend: backend::RouteBackendMetrics<B>,
}

pub type HttpRouteMetrics = RouteMetrics<LabelHttpRouteRsp, LabelHttpRouteBackendRsp>;
pub type GrpcRouteMetrics = RouteMetrics<LabelGrpcRouteRsp, LabelGrpcRouteBackendRsp>;

/// Tracks HTTP streams to produce response labels.
#[derive(Clone, Debug)]
pub struct LabelHttpRsp<L> {
    parent: L,
    status: Option<http::StatusCode>,
    error: Option<labels::Error>,
}

/// Tracks gRPC streams to produce response labels.
#[derive(Clone, Debug)]
pub struct LabelGrpcRsp<L> {
    parent: L,
    status: Option<tonic::Code>,
    error: Option<labels::Error>,
}

pub type LabelHttpRouteRsp = LabelHttpRsp<labels::Route>;
pub type LabelGrpcRouteRsp = LabelGrpcRsp<labels::Route>;

pub type LabelHttpRouteBackendRsp = LabelHttpRsp<labels::RouteBackend>;
pub type LabelGrpcRouteBackendRsp = LabelGrpcRsp<labels::RouteBackend>;

pub type NewRecordDuration<T, M, N> =
    record_response::NewRecordResponse<T, ExtractRecordDurationParams<M>, M, N>;

#[derive(Clone, Debug)]
pub struct ExtractRecordDurationParams<M>(M);

pub fn request_duration<T, N>(
    metric: RequestMetrics<T::DurationLabels, T::TotalLabels>,
) -> impl svc::Layer<
    N,
    Service = NewRecordDuration<T, RequestMetrics<T::DurationLabels, T::TotalLabels>, N>,
> + Clone
where
    T: Clone + MkStreamLabel,
{
    NewRecordDuration::layer_via(ExtractRecordDurationParams(metric))
}

pub fn response_duration<T, N>(
    metric: ResponseMetrics<T::DurationLabels, T::TotalLabels>,
) -> impl svc::Layer<
    N,
    Service = NewRecordDuration<T, ResponseMetrics<T::DurationLabels, T::TotalLabels>, N>,
> + Clone
where
    T: Clone + MkStreamLabel,
{
    NewRecordDuration::layer_via(ExtractRecordDurationParams(metric))
}

// === impl RouteMetrics ===

impl<R: StreamLabel, B: StreamLabel> Default for RouteMetrics<R, B> {
    fn default() -> Self {
        Self {
            requests: Default::default(),
            backend: Default::default(),
            retry: Default::default(),
        }
    }
}

impl<R: StreamLabel, B: StreamLabel> Clone for RouteMetrics<R, B> {
    fn clone(&self) -> Self {
        Self {
            requests: self.requests.clone(),
            backend: self.backend.clone(),
            retry: self.retry.clone(),
        }
    }
}

impl<R: StreamLabel, B: StreamLabel> RouteMetrics<R, B> {
    pub fn register(reg: &mut prom::Registry) -> Self {
        let requests = RequestMetrics::register(reg);

        let backend =
            backend::RouteBackendMetrics::register(reg.sub_registry_with_prefix("backend"));

        let retry = retry::RouteRetryMetrics::register(reg.sub_registry_with_prefix("retry"));

        Self {
            requests,
            backend,
            retry,
        }
    }

    // #[cfg(test)]
    // pub(crate) fn backend_metrics(
    //     &self,
    //     p: crate::ParentRef,
    //     r: RouteRef,
    //     b: crate::BackendRef,
    // ) -> backend::BackendHttpMetrics {
    //     self.backend.get(p, r, b)
    // }
}

// === impl ExtractRequestDurationParams ===

impl<T, M> svc::ExtractParam<record_response::Params<T, M>, T> for ExtractRecordDurationParams<M>
where
    T: Clone + MkStreamLabel,
    M: Clone,
{
    fn extract_param(&self, target: &T) -> record_response::Params<T, M> {
        record_response::Params {
            labeler: target.clone(),
            metric: self.0.clone(),
        }
    }
}

// === impl LabelHttpRsp ===

impl<P> From<P> for LabelHttpRsp<P> {
    fn from(parent: P) -> Self {
        Self {
            parent,
            status: None,
            error: None,
        }
    }
}

impl<P> StreamLabel for LabelHttpRsp<P>
where
    P: EncodeLabelSetMut + Clone + Eq + std::fmt::Debug + std::hash::Hash + Send + Sync + 'static,
{
    type DurationLabels = P;
    type TotalLabels = labels::Rsp<P, labels::HttpRsp>;

    fn init_response<B>(&mut self, rsp: &http::Response<B>) {
        self.status = Some(rsp.status());
    }

    fn end_response(&mut self, res: Result<Option<&http::HeaderMap>, &linkerd_app_core::Error>) {
        // TODO handle H2 errors distinctly.
        self.error = res.err().map(labels::Error::new);
    }

    fn duration_labels(&self) -> Self::DurationLabels {
        self.parent.clone()
    }

    fn total_labels(&self) -> Self::TotalLabels {
        labels::Rsp(
            self.parent.clone(),
            labels::HttpRsp {
                status: self.status,
                error: self.error,
            },
        )
    }
}

// === impl LabelGrpcRsp ===

impl<P> From<P> for LabelGrpcRsp<P> {
    fn from(parent: P) -> Self {
        Self {
            parent,
            status: None,
            error: None,
        }
    }
}

impl<P> StreamLabel for LabelGrpcRsp<P>
where
    P: EncodeLabelSetMut + Clone + Eq + std::fmt::Debug + std::hash::Hash + Send + Sync + 'static,
{
    type DurationLabels = P;
    type TotalLabels = labels::Rsp<P, labels::GrpcRsp>;

    fn init_response<B>(&mut self, rsp: &http::Response<B>) {
        self.status = rsp
            .headers()
            .get("grpc-status")
            .map(|v| tonic::Code::from_bytes(v.as_bytes()));
    }

    fn end_response(&mut self, res: Result<Option<&http::HeaderMap>, &linkerd_app_core::Error>) {
        self.status = self.status.or_else(|| {
            let trailers = res.ok().flatten()?;
            trailers
                .get("grpc-status")
                .map(|v| tonic::Code::from_bytes(v.as_bytes()))
        });

        // TODO handle H2 errors distinctly.
        self.error = res.err().map(labels::Error::new);
    }

    fn duration_labels(&self) -> Self::DurationLabels {
        self.parent.clone()
    }

    fn total_labels(&self) -> Self::TotalLabels {
        labels::Rsp(
            self.parent.clone(),
            labels::GrpcRsp {
                status: self.status,
                error: self.error,
            },
        )
    }
}

/// Prometheus label types.
pub mod labels {
    use linkerd_app_core::{metrics::prom::EncodeLabelSetMut, Error as BoxError};
    use prometheus_client::encoding::*;

    use crate::{BackendRef, ParentRef, RouteRef};

    #[derive(Clone, Debug, Hash, PartialEq, Eq)]
    pub struct Route(pub ParentRef, pub RouteRef);

    #[derive(Clone, Debug, Hash, PartialEq, Eq)]
    pub struct RouteBackend(ParentRef, RouteRef, BackendRef);

    #[derive(Clone, Debug, Hash, PartialEq, Eq)]
    pub struct Rsp<P, L>(pub P, pub L);

    pub type RouteRsp<L> = Rsp<Route, L>;
    pub type HttpRouteRsp = RouteRsp<HttpRsp>;
    pub type GrpcRouteRsp = RouteRsp<GrpcRsp>;

    pub type RouteBackendRsp<L> = Rsp<RouteBackend, L>;
    pub type HttpRouteBackendRsp = RouteBackendRsp<HttpRsp>;
    pub type GrpcRouteBackendRsp = RouteBackendRsp<GrpcRsp>;

    #[derive(Clone, Debug, Hash, PartialEq, Eq)]
    pub struct HttpRsp {
        pub status: Option<http::StatusCode>,
        pub error: Option<Error>,
    }

    #[derive(Clone, Debug, Hash, PartialEq, Eq)]
    pub struct GrpcRsp {
        pub status: Option<tonic::Code>,
        pub error: Option<Error>,
    }

    #[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
    pub enum Error {
        Unknown,
    }

    // === impl Route ===

    impl From<(ParentRef, RouteRef)> for Route {
        fn from((parent, route): (ParentRef, RouteRef)) -> Self {
            Self(parent, route)
        }
    }

    impl EncodeLabelSetMut for Route {
        fn encode_label_set(&self, enc: &mut LabelSetEncoder<'_>) -> std::fmt::Result {
            let Self(parent, route) = self;
            parent.encode_label_set(enc)?;
            route.encode_label_set(enc)?;
            Ok(())
        }
    }

    impl EncodeLabelSet for Route {
        fn encode(&self, mut enc: LabelSetEncoder<'_>) -> std::fmt::Result {
            self.encode_label_set(&mut enc)
        }
    }

    // === impl RouteBackend ===

    impl From<(ParentRef, RouteRef, BackendRef)> for RouteBackend {
        fn from((parent, route, backend): (ParentRef, RouteRef, BackendRef)) -> Self {
            Self(parent, route, backend)
        }
    }

    impl EncodeLabelSetMut for RouteBackend {
        fn encode_label_set(&self, enc: &mut LabelSetEncoder<'_>) -> std::fmt::Result {
            let Self(parent, route, backend) = self;
            parent.encode_label_set(enc)?;
            route.encode_label_set(enc)?;
            backend.encode_label_set(enc)?;
            Ok(())
        }
    }

    impl EncodeLabelSet for RouteBackend {
        fn encode(&self, mut enc: LabelSetEncoder<'_>) -> std::fmt::Result {
            self.encode_label_set(&mut enc)
        }
    }
    // === impl Rsp ===

    impl<P: EncodeLabelSetMut, L: EncodeLabelSetMut> EncodeLabelSetMut for Rsp<P, L> {
        fn encode_label_set(&self, enc: &mut LabelSetEncoder<'_>) -> std::fmt::Result {
            let Self(route, rsp) = self;
            route.encode_label_set(enc)?;
            rsp.encode_label_set(enc)?;
            Ok(())
        }
    }

    impl<P: EncodeLabelSetMut, L: EncodeLabelSetMut> EncodeLabelSet for Rsp<P, L> {
        fn encode(&self, mut enc: LabelSetEncoder<'_>) -> std::fmt::Result {
            self.encode_label_set(&mut enc)
        }
    }

    // === impl HttpRsp ===

    impl EncodeLabelSetMut for HttpRsp {
        fn encode_label_set(&self, enc: &mut LabelSetEncoder<'_>) -> std::fmt::Result {
            let Self { status, error } = self;

            ("http_status", status.map(|c| c.as_u16())).encode(enc.encode_label())?;
            ("error", *error).encode(enc.encode_label())?;

            Ok(())
        }
    }

    impl EncodeLabelSet for HttpRsp {
        fn encode(&self, mut enc: LabelSetEncoder<'_>) -> std::fmt::Result {
            self.encode_label_set(&mut enc)
        }
    }

    // === impl GrpcRsp ===

    impl EncodeLabelSetMut for GrpcRsp {
        fn encode_label_set(&self, enc: &mut LabelSetEncoder<'_>) -> std::fmt::Result {
            let Self { status, error } = self;

            (
                "grpc_status",
                match status.unwrap_or(tonic::Code::Unknown) {
                    tonic::Code::Ok => "OK",
                    tonic::Code::Cancelled => "CANCELLED",
                    tonic::Code::InvalidArgument => "INVALID_ARGUMENT",
                    tonic::Code::DeadlineExceeded => "DEADLINE_EXCEEDED",
                    tonic::Code::NotFound => "NOT_FOUND",
                    tonic::Code::AlreadyExists => "ALREADY_EXISTS",
                    tonic::Code::PermissionDenied => "PERMISSION_DENIED",
                    tonic::Code::ResourceExhausted => "RESOURCE_EXHAUSTED",
                    tonic::Code::FailedPrecondition => "FAILED_PRECONDITION",
                    tonic::Code::Aborted => "ABORTED",
                    tonic::Code::OutOfRange => "OUT_OF_RANGE",
                    tonic::Code::Unimplemented => "UNIMPLEMENTED",
                    tonic::Code::Internal => "INTERNAL",
                    tonic::Code::Unavailable => "UNAVAILABLE",
                    tonic::Code::DataLoss => "DATA_LOSS",
                    tonic::Code::Unauthenticated => "UNAUTHENTICATED",
                    _ => "UNKNOWN",
                },
            )
                .encode(enc.encode_label())?;

            ("error", *error).encode(enc.encode_label())?;

            Ok(())
        }
    }

    impl EncodeLabelSet for GrpcRsp {
        fn encode(&self, mut enc: LabelSetEncoder<'_>) -> std::fmt::Result {
            self.encode_label_set(&mut enc)
        }
    }

    // === impl Error ===

    impl Error {
        pub fn new(_err: &BoxError) -> Self {
            Self::Unknown
        }
    }

    impl EncodeLabelValue for Error {
        fn encode(&self, enc: &mut LabelValueEncoder<'_>) -> std::fmt::Result {
            use std::fmt::Write;
            match self {
                Self::Unknown => enc.write_str("unknown"),
            }
        }
    }
}
