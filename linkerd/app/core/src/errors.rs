pub mod respond;

pub use self::respond::{HttpRescue, NewRespond, NewRespondService, SyntheticHttpResponse};
pub use linkerd_proxy_http::h2::H2Error;
pub use linkerd_stack::FailFastError;
use thiserror::Error;
pub use tonic::Code as Grpc;

#[derive(Debug, Error)]
#[error("connect timed out after {0:?}")]
pub struct ConnectTimeout(pub(crate) std::time::Duration);

/// Obtain the source error at the end of a chain of `Error`s.
pub fn root_cause<'e>(
    mut error: &'e (dyn std::error::Error + 'static),
) -> &'e (dyn std::error::Error + 'static) {
    while let Some(e) = error.source() {
        error = e;
    }
    error
}

/// Determines if the provided error was caused by an `E` typed error.
pub fn caused_by<'e, E: std::error::Error + 'static>(
    mut error: &'e (dyn std::error::Error + 'static),
) -> Option<&'e E> {
    while let Some(src) = error.source() {
        if let Some(e) = src.downcast_ref::<E>() {
            return Some(e);
        }
        error = src;
    }
    None
}
