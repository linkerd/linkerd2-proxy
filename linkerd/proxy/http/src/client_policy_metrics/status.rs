use linkerd_metrics::prom::{encoding::*, EncodeLabelSetMut};

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum StatusLabel {
    Http(http::StatusCode),
    Grpc(tonic::Code),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum StatusVariant {
    Http,
    Grpc,
}

impl Default for StatusVariant {
    fn default() -> Self {
        Self::Http
    }
}

// === impl StatusLabel ===

impl EncodeLabelSetMut for StatusLabel {
    fn encode_label_set(&self, dst: &mut LabelSetEncoder<'_>) -> std::fmt::Result {
        match self {
            Self::Http(code) => {
                ("http_status", code.as_str()).encode(dst.encode_label())?;
            }
            Self::Grpc(code) => {
                let status = match code {
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
                };
                ("grpc_status", status).encode(dst.encode_label())?;
            }
        }
        Ok(())
    }
}

impl EncodeLabelSet for StatusLabel {
    fn encode(&self, mut dst: LabelSetEncoder<'_>) -> std::fmt::Result {
        EncodeLabelSetMut::encode_label_set(self, &mut dst)
    }
}
