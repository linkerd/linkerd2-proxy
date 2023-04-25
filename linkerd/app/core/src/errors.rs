pub mod respond;

pub use self::respond::{HttpRescue, NewRespond, NewRespondService, SyntheticHttpResponse};
pub use linkerd_error::{cause_ref, is_caused_by};
pub use linkerd_proxy_http::h2::H2Error;
pub use linkerd_stack::{FailFastError, LoadShedError};
pub use tonic::Code as Grpc;

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
