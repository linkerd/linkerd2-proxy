use linkerd_app_core::{metrics::prom::EncodeLabelSetMut, svc};
use linkerd_http_prom::record_response::{self, StreamLabel};

pub use linkerd_http_prom::record_response::MkStreamLabel;

pub type RouteRequestDuration<RspL> = record_response::RequestDuration<labels::RouteRsp<RspL>>;

#[derive(Clone, Debug)]
pub struct ExtractRecordDurationParams<M>(M);

/// Tracks HTTP streams to produce response labels.
#[derive(Clone, Debug)]
pub struct LabelHttpRsp<L> {
    parent: L,
    status: Option<http::StatusCode>,
}

/// Tracks gRPC streams to produce response labels.
#[derive(Clone, Debug)]
pub struct LabelGrpcRsp<L> {
    parent: L,
    status: Option<tonic::Code>,
}

pub type LabelHttpRouteRsp = LabelHttpRsp<labels::Route>;
pub type LabelGrpcRouteRsp = LabelGrpcRsp<labels::Route>;

pub type NewRecordDuration<T, M, N> =
    record_response::NewRecordResponse<T, ExtractRecordDurationParams<M>, M, N>;

pub fn request_duration<T, RspL, N>(
    metric: RouteRequestDuration<RspL>,
) -> impl svc::Layer<N, Service = NewRecordDuration<T, RouteRequestDuration<RspL>, N>> + Clone
where
    T: Clone + MkStreamLabel<EncodeLabelSet = labels::RouteRsp<RspL>>,
    RspL:
        EncodeLabelSetMut + Clone + Eq + std::fmt::Debug + std::hash::Hash + Send + Sync + 'static,
{
    NewRecordDuration::layer_via(ExtractRecordDurationParams(metric))
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
        }
    }
}

impl<P> StreamLabel for LabelHttpRsp<P>
where
    P: EncodeLabelSetMut + Clone + Eq + std::fmt::Debug + std::hash::Hash + Send + Sync + 'static,
{
    type EncodeLabelSet = labels::Rsp<P, labels::HttpRsp>;

    fn init_response<B>(&mut self, rsp: &http::Response<B>) {
        self.status = Some(rsp.status());
    }

    fn end_response(
        self,
        res: Result<Option<&http::HeaderMap>, &linkerd_app_core::Error>,
    ) -> Self::EncodeLabelSet {
        // TODO handle H2 errors distinctly.
        let error = res.err().map(labels::Error::new);
        labels::Rsp(
            self.parent.clone(),
            labels::HttpRsp {
                status: self.status,
                error,
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
        }
    }
}

impl<P> StreamLabel for LabelGrpcRsp<P>
where
    P: EncodeLabelSetMut + Clone + Eq + std::fmt::Debug + std::hash::Hash + Send + Sync + 'static,
{
    type EncodeLabelSet = labels::Rsp<P, labels::GrpcRsp>;

    fn init_response<B>(&mut self, rsp: &http::Response<B>) {
        self.status = rsp
            .headers()
            .get("grpc-status")
            .map(|v| tonic::Code::from_bytes(v.as_bytes()));
    }

    fn end_response(
        self,
        res: Result<Option<&http::HeaderMap>, &linkerd_app_core::Error>,
    ) -> Self::EncodeLabelSet {
        let status = self.status.or_else(|| {
            let trailers = res.ok().flatten()?;
            trailers
                .get("grpc-status")
                .map(|v| tonic::Code::from_bytes(v.as_bytes()))
        });

        // TODO handle H2 errors distinctly.
        let error = res.err().map(labels::Error::new);

        labels::Rsp(self.parent.clone(), labels::GrpcRsp { status, error })
    }
}

/// Prometheus label types.
pub mod labels {
    use linkerd_app_core::metrics::prom::EncodeLabelSetMut;
    use linkerd_app_core::Error as BoxError;
    use prometheus_client::encoding::*;

    use crate::{ParentRef, RouteRef};

    #[derive(Clone, Debug, Hash, PartialEq, Eq)]
    pub struct Route(pub ParentRef, pub RouteRef);

    #[derive(Clone, Debug, Hash, PartialEq, Eq)]
    pub struct Rsp<P, L>(pub P, pub L);

    pub type RouteRsp<L> = Rsp<Route, L>;

    pub type HttpRouteRsp = RouteRsp<HttpRsp>;

    pub type GrpcRouteRsp = RouteRsp<GrpcRsp>;

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

    // === impl RouteRsp ===

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
