pub mod respond;

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

impl respond::Rescue<Error> for DefaultRescue {
    fn rescue(&self, error: Error) -> Result<respond::Rescued> {
        if Self::is_h2_error(&*error) {
            tracing::debug!(%error, "Propagating HTTP2 reset");
            return Err(error);
        }

        Ok(Self::mk(&*error))
    }
}

impl DefaultRescue {
    fn is_h2_error(error: &(dyn std::error::Error + 'static)) -> bool {
        error.is::<h2::H2Error>() || error.source().map(Self::is_h2_error).unwrap_or(false)
    }

    fn mk(error: &(dyn std::error::Error + 'static)) -> respond::Rescued {
        if error.is::<ConnectTimeout>() {
            return respond::Rescued {
                http_status: http::StatusCode::GATEWAY_TIMEOUT,
                grpc_status: tonic::Code::DeadlineExceeded,
                close_connection: true,
                message: error.to_string(),
            };
        }

        if error.is::<ResponseTimeout>() {
            return respond::Rescued {
                http_status: http::StatusCode::GATEWAY_TIMEOUT,
                grpc_status: tonic::Code::DeadlineExceeded,
                close_connection: false,
                message: error.to_string(),
            };
        }

        if error.is::<FailFastError>() {
            return respond::Rescued {
                http_status: http::StatusCode::SERVICE_UNAVAILABLE,
                grpc_status: tonic::Code::Unavailable,
                close_connection: true,
                message: error.to_string(),
            };
        }

        if error.is::<orig_proto::DowngradedH2Error>() {
            return respond::Rescued {
                http_status: http::StatusCode::BAD_GATEWAY,
                grpc_status: tonic::Code::Unavailable,
                close_connection: true,
                message: error.to_string(),
            };
        }

        if let Some(source) = error.source() {
            return Self::mk(source);
        }

        respond::Rescued::default()
    }
}
