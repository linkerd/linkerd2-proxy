pub mod respond;

pub use self::respond::{HttpRescue, NewRespond, NewRespondService, SyntheticHttpResponse};
pub use linkerd_error::{cause_ref, is_caused_by};
pub use linkerd_proxy_http::h2::H2Error;
pub use linkerd_stack::{FailFastError, LoadShedError};
pub use tonic::Code as Grpc;

/// Header names and values related to error responses.
pub mod header {
    use http::header::{HeaderName, HeaderValue};
    pub const L5D_PROXY_CONNECTION: HeaderName = HeaderName::from_static("l5d-proxy-connection");
    pub const L5D_PROXY_ERROR: HeaderName = HeaderName::from_static("l5d-proxy-error");
    pub(super) const GRPC_CONTENT_TYPE: HeaderValue = HeaderValue::from_static("application/grpc");
    pub(super) const GRPC_MESSAGE: HeaderName = HeaderName::from_static("grpc-message");
    pub(super) const GRPC_STATUS: HeaderName = HeaderName::from_static("grpc-status");
}

#[derive(Debug, thiserror::Error)]
#[error("connect timed out after {0:?}")]
pub struct ConnectTimeout(pub(crate) std::time::Duration);

/// Returns `true` if `error` was caused by a gRPC error with the provided
/// status code.
#[inline]
pub fn has_grpc_status(error: &crate::Error, code: tonic::Code) -> bool {
    cause_ref::<tonic::Status>(error.as_ref())
        .map(|s| s.code() == code)
        .unwrap_or(false)
}

// Copied from tonic, where it's private.
fn code_header(code: tonic::Code) -> http::HeaderValue {
    use {http::HeaderValue, tonic::Code};
    match code {
        Code::Ok => HeaderValue::from_static("0"),
        Code::Cancelled => HeaderValue::from_static("1"),
        Code::Unknown => HeaderValue::from_static("2"),
        Code::InvalidArgument => HeaderValue::from_static("3"),
        Code::DeadlineExceeded => HeaderValue::from_static("4"),
        Code::NotFound => HeaderValue::from_static("5"),
        Code::AlreadyExists => HeaderValue::from_static("6"),
        Code::PermissionDenied => HeaderValue::from_static("7"),
        Code::ResourceExhausted => HeaderValue::from_static("8"),
        Code::FailedPrecondition => HeaderValue::from_static("9"),
        Code::Aborted => HeaderValue::from_static("10"),
        Code::OutOfRange => HeaderValue::from_static("11"),
        Code::Unimplemented => HeaderValue::from_static("12"),
        Code::Internal => HeaderValue::from_static("13"),
        Code::Unavailable => HeaderValue::from_static("14"),
        Code::DataLoss => HeaderValue::from_static("15"),
        Code::Unauthenticated => HeaderValue::from_static("16"),
    }
}
