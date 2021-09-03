pub mod respond;

use linkerd_error::Error;
pub use linkerd_timeout::{FailFastError, ResponseTimeout};
use thiserror::Error;

#[derive(Debug, Error)]
#[error("connect timed out after {0:?}")]
pub(crate) struct ConnectTimeout(pub std::time::Duration);

#[derive(Debug, Error)]
#[error("{source}")]
pub struct HttpError {
    #[source]
    source: Error,
    http_status: http::StatusCode,
    grpc_status: tonic::Code,
}

impl HttpError {
    pub fn bad_request(source: impl Into<Error>) -> Self {
        Self {
            source: source.into(),
            http_status: http::StatusCode::BAD_REQUEST,
            grpc_status: tonic::Code::InvalidArgument,
        }
    }

    pub fn forbidden(source: impl Into<Error>) -> Self {
        Self {
            source: source.into(),
            http_status: http::StatusCode::FORBIDDEN,
            grpc_status: tonic::Code::PermissionDenied,
        }
    }

    pub fn loop_detected(source: impl Into<Error>) -> Self {
        Self {
            source: source.into(),
            http_status: http::StatusCode::LOOP_DETECTED,
            grpc_status: tonic::Code::Aborted,
        }
    }
}
