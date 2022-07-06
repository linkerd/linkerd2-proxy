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
    use linkerd2_proxy_api::grpc_route as api;

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidFailureResponse {
        #[error("invalid gRPC status code: {0}")]
        StatusNonU16(u32),

        #[error("invalid request distribution: {0}")]
        Distribution(#[from] rand::distributions::BernoulliError),
    }

    // === impl InjectFailure ===

    impl TryFrom<api::GrpcFailureInjector> for InjectFailure {
        type Error = InvalidFailureResponse;

        fn try_from(proto: api::GrpcFailureInjector) -> Result<Self, Self::Error> {
            if proto.code > u16::MAX as u32 {
                return Err(InvalidFailureResponse::StatusNonU16(proto.code));
            }
            let response = FailureResponse {
                code: proto.code as u16,
                message: proto.message.into(),
            };

            let distribution = match proto.ratio {
                Some(r) => r.try_into()?,
                None => Default::default(),
            };

            Ok(InjectFailure {
                response,
                distribution,
            })
        }
    }
}
