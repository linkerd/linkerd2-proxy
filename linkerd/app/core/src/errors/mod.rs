pub mod respond;

pub use self::respond::{HttpRescue, SyntheticHttpResponse};
use linkerd_error::{Error, Result};
use linkerd_proxy_http::h2;
pub use linkerd_stack::{FailFastError, TimeoutError};
use thiserror::Error;
pub use tonic::Code as Grpc;

#[derive(Debug, Error)]
#[error("connect timed out after {0:?}")]
pub struct ConnectTimeout(pub(crate) std::time::Duration);

#[derive(Clone, Debug)]
pub struct DefaultHttpRescue;

// === impl DefaultHttpRescue ===

impl DefaultHttpRescue {
    pub fn layer() -> respond::Layer<Self> {
        respond::NewRespond::layer(Self)
    }
}

impl HttpRescue<Error> for DefaultHttpRescue {
    fn rescue(&self, error: Error) -> Result<SyntheticHttpResponse> {
        if Self::has_cause::<h2::H2Error>(&*error) {
            tracing::debug!(%error, "Propagating HTTP2 reset");
            return Err(error);
        }

        if Self::has_cause::<FailFastError>(&*error) {
            return Ok(SyntheticHttpResponse {
                http_status: http::StatusCode::GATEWAY_TIMEOUT,
                grpc_status: tonic::Code::Unavailable,
                close_connection: true,
                message: error.to_string(),
            });
        }

        tracing::warn!(%error, "Unexpected error");
        Ok(SyntheticHttpResponse::default())
    }
}
