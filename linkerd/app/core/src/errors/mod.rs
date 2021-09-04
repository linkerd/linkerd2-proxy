pub mod respond;

use linkerd_error::{Error, Result};
use linkerd_proxy_http::ClientHandle;
pub use linkerd_timeout::{FailFastError, ResponseTimeout};
use thiserror::Error;

#[derive(Debug, Error)]
#[error("connect timed out after {0:?}")]
pub(crate) struct ConnectTimeout(pub std::time::Duration);

#[derive(Clone, Debug)]
pub struct DefaultRescue;

#[derive(Debug, Error)]
#[error("{source}")]
pub struct HttpError {
    #[source]
    source: Error,
    http_status: http::StatusCode,
    grpc_status: tonic::Code,
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

impl respond::Rescue for DefaultRescue {
    fn rescue(
        &self,
        version: http::Version,
        error: Error,
        client: Option<&ClientHandle>,
    ) -> Result<respond::Rescued> {
        use linkerd_proxy_http::HasH2Reason;

        let addr = client
            .map(|ClientHandle { ref addr, .. }| *addr)
            .unwrap_or_else(|| {
                tracing::debug!("Missing client address");
                ([0, 0, 0, 0], 0).into()
            });
        tracing::info!(%addr, %error, "Request failed");

        if version == http::Version::HTTP_2 {
            if let Some(reset) = error.h2_reason() {
                tracing::debug!(%reset, "Propagating HTTP2 reset");
                return Err(error);
            }
        }

        // Gracefully teardown the server-side connection.
        if let Some(ClientHandle { ref close, .. }) = client {
            tracing::debug!("Closing server-side connection");
            close.close();
        }

        Ok(Self::mk(&*error))
    }
}

impl DefaultRescue {
    fn mk(error: &(dyn std::error::Error + 'static)) -> respond::Rescued {
        if error.is::<ConnectTimeout>() || error.is::<ResponseTimeout>() {
            return respond::Rescued {
                http_status: http::StatusCode::GATEWAY_TIMEOUT,
                grpc_status: tonic::Code::DeadlineExceeded,
                message: error.to_string(),
            };
        }

        if error.is::<FailFastError>() {
            return respond::Rescued {
                http_status: http::StatusCode::SERVICE_UNAVAILABLE,
                grpc_status: tonic::Code::Unavailable,
                message: error.to_string(),
            };
        }

        if let Some(HttpError {
            source,
            http_status,
            grpc_status,
        }) = error.downcast_ref()
        {
            return respond::Rescued {
                http_status: *http_status,
                grpc_status: *grpc_status,
                message: source.to_string(),
            };
        }

        if let Some(source) = error.source() {
            return Self::mk(source);
        }

        respond::Rescued {
            http_status: http::StatusCode::BAD_GATEWAY,
            grpc_status: tonic::Code::Internal,
            message: "unexpected error".to_string(),
        }
    }
}
