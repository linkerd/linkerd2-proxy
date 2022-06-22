use crate::{
    filter::RespondWithError,
    r#match::{MatchRequest, MatchRpc},
};
use linkerd2_proxy_api::grpc_route as api;
use linkerd_http_route as http_route;

// === impl MatchRequest ===

#[derive(Debug, thiserror::Error)]
pub enum RouteMatchError {
    #[error("invalid RPC match: {0}")]
    Rpc(#[from] RpcMatchError),

    #[error("invalid header match: {0}")]
    Header(#[from] http_route::proto::HeaderMatchError),
}

impl TryFrom<api::GrpcRouteMatch> for MatchRequest {
    type Error = RouteMatchError;

    fn try_from(rm: api::GrpcRouteMatch) -> Result<Self, Self::Error> {
        let rpc = rm
            .rpc
            .ok_or(RouteMatchError::Rpc(RpcMatchError::Missing))?
            .try_into()?;

        Ok(MatchRequest {
            rpc,
            ..MatchRequest::default()
        })
    }
}

// === impl MatchPath ===

#[derive(Debug, thiserror::Error)]
pub enum RpcMatchError {
    #[error("missing RPC match")]
    Missing,
}

impl TryFrom<api::GrpcRpcMatch> for MatchRpc {
    type Error = RpcMatchError;

    fn try_from(rm: api::GrpcRpcMatch) -> Result<Self, Self::Error> {
        Ok(MatchRpc {
            service: if rm.service.is_empty() {
                None
            } else {
                Some(rm.service)
            },
            method: if rm.method.is_empty() {
                None
            } else {
                Some(rm.method)
            },
        })
    }
}

// === impl RespondWithError ===

#[derive(Debug, thiserror::Error)]
pub enum ErrorResponderError {
    #[error("invalid HTTP status code: {0}")]
    InvalidStatusNonU16(u32),
}

impl TryFrom<api::GrpcErrorResponder> for RespondWithError {
    type Error = ErrorResponderError;

    fn try_from(proto: api::GrpcErrorResponder) -> Result<Self, Self::Error> {
        if proto.code > u16::MAX as u32 {
            return Err(ErrorResponderError::InvalidStatusNonU16(proto.code));
        }

        Ok(RespondWithError {
            code: proto.code as u16,
            message: proto.message.into(),
        })
    }
}
