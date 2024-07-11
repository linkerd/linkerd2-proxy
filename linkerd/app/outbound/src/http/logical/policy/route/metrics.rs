use linkerd_app_core::metrics::prom::EncodeLabelSetMut;
use linkerd_http_prom::record_response::{self, StreamLabel};

pub type RequestDuration<RspL> = record_response::RequestDuration<labels::RouteRsp<RspL>>;
// pub type HttpRouteRequestDuration = RequestDuration<labels::HttpRsp>;
// pub type GrpcRouteRequestDuration = RequestDuration<labels::GrpcRsp>;

pub type RequestDurationParams<T, RspL> = record_response::Params<T, RequestDuration<RspL>>;
pub type HttpRouteRequestDurationParams<T> = RequestDurationParams<T, labels::HttpRsp>;
pub type GrpcRouteRequestDurationParams<T> = RequestDurationParams<T, labels::GrpcRsp>;

#[derive(Clone, Debug)]
pub struct LabelHttpRsp<L> {
    parent: L,
    status: Option<http::StatusCode>,
}

#[derive(Clone, Debug)]
pub struct LabelGrpcRsp<L> {
    parent: L,
    status: Option<tonic::Code>,
}

pub type LabelHttpRouteRsp = LabelHttpRsp<labels::Route>;
pub type LabelGrpcRouteRsp = LabelGrpcRsp<labels::Route>;

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
        trailers: Result<Option<&http::HeaderMap>, &linkerd_app_core::Error>,
    ) -> Self::EncodeLabelSet {
        let error = trailers.err().map(labels::Error::new);
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
        trailers: Result<Option<&http::HeaderMap>, &linkerd_app_core::Error>,
    ) -> Self::EncodeLabelSet {
        let status = self.status.or_else(|| {
            trailers
                .ok()
                .flatten()?
                .get("grpc-status")
                .map(|v| tonic::Code::from_bytes(v.as_bytes()))
        });

        let error = trailers.err().map(labels::Error::new);

        labels::Rsp(self.parent.clone(), labels::GrpcRsp { status, error })
    }
}

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
                match status {
                    Some(tonic::Code::Ok) => "ok",
                    Some(tonic::Code::Cancelled) => "cancelled",
                    Some(tonic::Code::Unknown) | None => "unknown",
                    Some(tonic::Code::InvalidArgument) => "invalid_argument",
                    Some(tonic::Code::DeadlineExceeded) => "deadline_exceeded",
                    Some(tonic::Code::NotFound) => "not_found",
                    Some(tonic::Code::AlreadyExists) => "already_exists",
                    Some(tonic::Code::PermissionDenied) => "permission_denied",
                    Some(tonic::Code::ResourceExhausted) => "resource-exhausted",
                    Some(tonic::Code::FailedPrecondition) => "failed-precondition",
                    Some(tonic::Code::Aborted) => "aborted",
                    Some(tonic::Code::OutOfRange) => "out_of_range",
                    Some(tonic::Code::Unimplemented) => "unimplemented",
                    Some(tonic::Code::Internal) => "internal",
                    Some(tonic::Code::Unavailable) => "unavailable",
                    Some(tonic::Code::DataLoss) => "data_loss",
                    Some(tonic::Code::Unauthenticated) => "unauthenticated",
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
