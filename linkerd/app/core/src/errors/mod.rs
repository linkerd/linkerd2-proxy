pub mod respond;

use linkerd_error::Result;
use linkerd_proxy_http::{h2, HasH2Reason};
pub use linkerd_timeout::{FailFastError, ResponseTimeout};
use thiserror::Error;

#[derive(Debug, Error)]
#[error("connect timed out after {0:?}")]
pub(crate) struct ConnectTimeout(pub std::time::Duration);

#[derive(Clone, Debug)]
pub struct DefaultRescue;

// === impl DefaultRescue ===

impl<E: std::error::Error + HasH2Reason + 'static> respond::Rescue<E> for DefaultRescue {
    fn rescue(&self, version: http::Version, error: E) -> Result<Option<respond::Rescued>, E> {
        if let Some(reset) = error.h2_reason() {
            tracing::debug!(%reset, "Propagating HTTP2 reset");
            return Err(error);
        }

        Ok(Self::mk(&error))
    }
}

impl DefaultRescue {
    fn mk(error: &(dyn std::error::Error + 'static)) -> Option<respond::Rescued> {
        if error.is::<ConnectTimeout>() || error.is::<ResponseTimeout>() {
            return Some(respond::Rescued {
                http_status: http::StatusCode::GATEWAY_TIMEOUT,
                grpc_status: tonic::Code::DeadlineExceeded,
                close_connection: true,
                message: error.to_string(),
            });
        }

        if error.is::<FailFastError>() {
            return Some(respond::Rescued {
                http_status: http::StatusCode::SERVICE_UNAVAILABLE,
                grpc_status: tonic::Code::Unavailable,
                close_connection: true,
                message: error.to_string(),
            });
        }

        if let Some(source) = error.source() {
            return Self::mk(source);
        }

        None
    }
}
