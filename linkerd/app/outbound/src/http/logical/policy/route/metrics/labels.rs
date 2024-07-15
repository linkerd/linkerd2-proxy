//! Prometheus label types.
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
