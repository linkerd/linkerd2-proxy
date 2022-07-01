use std::hash::Hash;

pub use crate::http::filter::Distribution;

pub type InjectFailure = crate::http::filter::InjectFailure<FailureResponse>;

/// A gRPC error code and status message.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct FailureResponse {
    pub code: u16,
    pub message: std::sync::Arc<str>,
}
