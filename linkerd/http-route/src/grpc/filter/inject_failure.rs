use std::hash::Hash;

pub use crate::http::filter::Distribution;

pub type InjectFailure = crate::http::filter::InjectFailure<FailureResponse>;

/// A gRPC error code and status message.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct FailureResponse {
    pub code: u16,
    pub message: std::sync::Arc<str>,
}

#[cfg(feature = "proto")]
pub mod proto {
    use super::*;
    pub use crate::http::filter::inject_failure::proto::InvalidDistribution;
    use linkerd2_proxy_api::grpc_route as api;

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidFailureResponse {
        #[error("gRPC status code is not a u16")]
        StatusNonU16(#[from] std::num::TryFromIntError),

        #[error("{0}")]
        Distribution(#[from] InvalidDistribution),
    }

    // === impl InjectFailure ===

    impl TryFrom<api::GrpcFailureInjector> for InjectFailure {
        type Error = InvalidFailureResponse;

        fn try_from(proto: api::GrpcFailureInjector) -> Result<Self, Self::Error> {
            let response = FailureResponse {
                code: u16::try_from(proto.code)?,
                message: proto.message.into(),
            };

            Ok(InjectFailure {
                response,
                distribution: proto.ratio.try_into()?,
            })
        }
    }
}
