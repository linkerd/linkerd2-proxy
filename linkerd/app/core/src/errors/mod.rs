pub mod respond;

pub use self::respond::{Rescue, SyntheticResponse};
use linkerd_error::{Error, Result};
use linkerd_proxy_http::{h2, orig_proto};
pub use linkerd_timeout::{FailFastError, ResponseTimeout};
use thiserror::Error;

#[derive(Debug, Error)]
#[error("connect timed out after {0:?}")]
pub(crate) struct ConnectTimeout(pub std::time::Duration);

#[derive(Clone, Debug)]
pub struct DefaultRescue;

// === impl DefaultRescue ===

impl DefaultRescue {
    pub fn layer() -> respond::Layer<Self> {
        respond::NewRespond::layer(Self)
    }
}

impl Rescue<Error> for DefaultRescue {
    fn rescue(&self, error: Error) -> Result<SyntheticResponse> {
        if Self::has_cause::<h2::H2Error>(&*error) {
            tracing::debug!(%error, "Propagating HTTP2 reset");
            return Err(error);
        }

        if Self::has_cause::<ConnectTimeout>(&*error) {
            return Ok(SyntheticResponse {
                http_status: http::StatusCode::GATEWAY_TIMEOUT,
                grpc_status: tonic::Code::DeadlineExceeded,
                close_connection: true,
                message: error.to_string(),
            });
        }

        if Self::has_cause::<ResponseTimeout>(&*error) {
            return Ok(SyntheticResponse {
                http_status: http::StatusCode::GATEWAY_TIMEOUT,
                grpc_status: tonic::Code::DeadlineExceeded,
                close_connection: false,
                message: error.to_string(),
            });
        }

        if Self::has_cause::<FailFastError>(&*error) {
            return Ok(SyntheticResponse {
                http_status: http::StatusCode::SERVICE_UNAVAILABLE,
                grpc_status: tonic::Code::Unavailable,
                close_connection: true,
                message: error.to_string(),
            });
        }

        if Self::has_cause::<orig_proto::DowngradedH2Error>(&*error) {
            return Ok(SyntheticResponse {
                http_status: http::StatusCode::BAD_GATEWAY,
                grpc_status: tonic::Code::Unavailable,
                close_connection: true,
                message: error.to_string(),
            });
        }

        Ok(SyntheticResponse::default())
    }
}
