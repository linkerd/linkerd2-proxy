//! Prometheus label types.
use linkerd_app_core::{
    dns, errors, metrics::prom::EncodeLabelSetMut, proxy::http, Error as BoxError,
};
use prometheus_client::encoding::*;

use crate::{BackendRef, ParentRef, RouteRef};

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct HttpRoute {
    parent: ParentRef,
    route: RouteRef,
    name: Option<dns::Name>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct GrpcRoute(pub ParentRef, pub RouteRef);

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct RouteBackend(pub ParentRef, pub RouteRef, pub BackendRef);

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct Rsp<P, L>(pub P, pub L);

pub type HttpRouteRsp = Rsp<HttpRoute, HttpRsp>;
pub type GrpcRouteRsp = Rsp<GrpcRoute, GrpcRsp>;

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
    FailFast,
    LoadShed,
    RequestTimeout,
    ResponseHeadersTimeout,
    ResponseStreamTimeout,
    IdleTimeout,
    Cancel,
    Refused,
    EnhanceYourCalm,
    Reset,
    GoAway,
    Io,
    Unknown,
}

// === impl HttpRoute ===

impl HttpRoute {
    pub fn new(parent: ParentRef, route: RouteRef, uri: &http::uri::Uri) -> Self {
        let name = uri
            .host()
            .map(str::as_bytes)
            .map(dns::Name::try_from_ascii)
            .and_then(Result::ok);

        Self {
            parent,
            route,
            name,
        }
    }

    #[cfg(test)]
    pub(super) fn new_with_name(
        parent: ParentRef,
        route: RouteRef,
        name: Option<dns::Name>,
    ) -> Self {
        Self {
            parent,
            route,
            name,
        }
    }
}

impl EncodeLabelSetMut for HttpRoute {
    fn encode_label_set(&self, enc: &mut LabelSetEncoder<'_>) -> std::fmt::Result {
        let Self {
            parent,
            route,
            name,
        } = self;

        parent.encode_label_set(enc)?;
        route.encode_label_set(enc)?;
        ("hostname", name.as_deref()).encode(enc.encode_label())?;

        Ok(())
    }
}

impl EncodeLabelSet for HttpRoute {
    fn encode(&self, mut enc: LabelSetEncoder<'_>) -> std::fmt::Result {
        self.encode_label_set(&mut enc)
    }
}

// === impl GrpcRoute ===

impl From<(ParentRef, RouteRef)> for GrpcRoute {
    fn from((parent, route): (ParentRef, RouteRef)) -> Self {
        Self(parent, route)
    }
}

impl EncodeLabelSetMut for GrpcRoute {
    fn encode_label_set(&self, enc: &mut LabelSetEncoder<'_>) -> std::fmt::Result {
        let Self(parent, route) = self;
        parent.encode_label_set(enc)?;
        route.encode_label_set(enc)?;
        Ok(())
    }
}

impl EncodeLabelSet for GrpcRoute {
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
    pub fn new_or_status(error: &BoxError) -> Result<Self, u16> {
        use super::super::super::errors as policy;
        use crate::http::h2::{H2Error, Reason};

        // No available backend can be found for a request.
        if errors::is_caused_by::<errors::FailFastError>(&**error) {
            return Ok(Self::FailFast);
        }
        if errors::is_caused_by::<errors::LoadShedError>(&**error) {
            return Ok(Self::LoadShed);
        }

        if let Some(policy::HttpRouteRedirect { status, .. }) = errors::cause_ref(&**error) {
            return Err(status.as_u16());
        }

        // Policy-driven request failures.
        if let Some(policy::HttpRouteInjectedFailure { status, .. }) = errors::cause_ref(&**error) {
            return Err(status.as_u16());
        }
        if let Some(policy::GrpcRouteInjectedFailure { code, .. }) = errors::cause_ref(&**error) {
            return Err(*code);
        }

        use http::stream_timeouts::{
            ResponseHeadersTimeoutError, ResponseStreamTimeoutError, StreamDeadlineError,
            StreamIdleError,
        };
        if errors::is_caused_by::<ResponseHeadersTimeoutError>(&**error) {
            return Ok(Self::ResponseHeadersTimeout);
        }
        if errors::is_caused_by::<ResponseStreamTimeoutError>(&**error) {
            return Ok(Self::ResponseStreamTimeout);
        }
        if errors::is_caused_by::<StreamDeadlineError>(&**error) {
            return Ok(Self::RequestTimeout);
        }
        if errors::is_caused_by::<StreamIdleError>(&**error) {
            return Ok(Self::IdleTimeout);
        }

        // HTTP/2 errors.
        if let Some(h2e) = errors::cause_ref::<H2Error>(&**error) {
            if h2e.is_reset() {
                match h2e.reason() {
                    Some(Reason::CANCEL) => return Ok(Self::Cancel),
                    Some(Reason::REFUSED_STREAM) => return Ok(Self::Refused),
                    Some(Reason::ENHANCE_YOUR_CALM) => return Ok(Self::EnhanceYourCalm),
                    _ => return Ok(Self::Reset),
                }
            }
            if h2e.is_go_away() {
                return Ok(Self::GoAway);
            }
            if h2e.is_io() {
                return Ok(Self::Io);
            }
        }

        tracing::debug!(?error, "Unlabeled error");
        Ok(Self::Unknown)
    }
}

impl EncodeLabelValue for Error {
    fn encode(&self, enc: &mut LabelValueEncoder<'_>) -> std::fmt::Result {
        use std::fmt::Write;
        match self {
            Self::FailFast => enc.write_str("FAIL_FAST"),
            Self::LoadShed => enc.write_str("LOAD_SHED"),
            Self::RequestTimeout => enc.write_str("REQUEST_TIMEOUT"),
            Self::ResponseHeadersTimeout => enc.write_str("RESPONSE_HEADERS_TIMEOUT"),
            Self::ResponseStreamTimeout => enc.write_str("RESPONSE_STREAM_TIMEOUT"),
            Self::IdleTimeout => enc.write_str("IDLE_TIMEOUT"),
            Self::Cancel => enc.write_str("CANCEL"),
            Self::Refused => enc.write_str("REFUSED"),
            Self::EnhanceYourCalm => enc.write_str("ENHANCE_YOUR_CALM"),
            Self::Reset => enc.write_str("RESET"),
            Self::GoAway => enc.write_str("GO_AWAY"),
            Self::Io => enc.write_str("IO"),
            Self::Unknown => enc.write_str("UNKNOWN"),
        }
    }
}
