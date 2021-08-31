pub mod respond;

use linkerd_error::Error;
use linkerd_proxy_transport::addrs::*;
pub use linkerd_timeout::{FailFastError, ResponseTimeout};
use linkerd_tls as tls;
use thiserror::Error;

#[derive(Debug, Error)]
#[error("connect timed out after {0:?}")]
pub(crate) struct ConnectTimeout(pub std::time::Duration);

#[derive(Debug, Error)]
#[error("{source}")]
pub struct HttpError {
    #[source]
    pub source: Error,
    pub http_status: http::StatusCode,
    pub grpc_status: tonic::Code,
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

#[derive(Debug, Error)]
#[error("required id {required:?}; found {found:?}")]
pub struct OutboundIdentityRequired {
    pub required: tls::client::ServerId,
    pub found: Option<tls::client::ServerId>,
}

#[derive(Debug, Error)]
#[error("no identity provided")]
pub struct GatewayIdentityRequired;

#[derive(Debug, Error)]
#[error("bad gateway domain")]
pub struct GatewayDomainInvalid;

#[derive(Debug, Error)]
#[error("gateway loop detected")]
pub struct GatewayLoop;

#[derive(Clone, Debug, Error)]
#[error("connection denied on unknown port {0}")]
pub struct DeniedUnknownPort(pub u16);

#[derive(Debug, Error)]
#[error("unauthorized connection from {client_addr} with identity {tls:?} to {dst_addr}")]
pub struct DeniedUnauthorized {
    pub client_addr: Remote<ClientAddr>,
    pub dst_addr: OrigDstAddr,
    pub tls: linkerd_tls::ConditionalServerTls,
}
