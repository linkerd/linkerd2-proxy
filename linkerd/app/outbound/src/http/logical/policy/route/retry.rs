use super::extensions;
use linkerd_app_core::{
    cause_ref, classify,
    exp_backoff::ExponentialBackoff,
    is_caused_by,
    proxy::http::{self, stream_timeouts::ResponseTimeoutError},
    svc, Error, Result,
};
use linkerd_http_retry::{self as retry, with_trailers::TrailersBody};
use linkerd_proxy_client_policy as policy;
use tokio::time;

// A request extension that marks that a request is a retry.
#[derive(Copy, Clone, Debug)]
pub struct IsRetry(());

pub type HttpRetry<S> = retry::HttpRetry<RetryPolicy, S>;

#[derive(Clone, Debug)]
pub struct RetryPolicy {
    pub max_retries: u16,
    pub max_request_bytes: usize,
    pub timeout: Option<time::Duration>,
    pub backoff: Option<ExponentialBackoff>,
    pub retryable_http_statuses: Option<policy::http::StatusRanges>,
    pub retryable_grpc_statuses: Option<policy::grpc::Codes>,
}

// === impl RetryPolicy ===

impl svc::Param<retry::Params> for RetryPolicy {
    fn param(&self) -> retry::Params {
        retry::Params {
            max_request_bytes: self.max_request_bytes,
        }
    }
}

impl retry::Policy for RetryPolicy {
    type Future = time::Sleep;

    fn exhausted(&self) -> bool {
        self.max_retries == 0
    }

    fn set_extensions(&self, dst: &mut ::http::Extensions, src: &::http::Extensions) {
        if let Some(extensions::Attempt(n)) = src.get::<extensions::Attempt>() {
            dst.insert(extensions::Attempt(n.saturating_add(1)));
        }

        let retries_remain = self.max_retries > 0;
        if retries_remain {
            dst.insert(self.clone());
        }

        if let Some(mut timeouts) = src.get::<http::StreamTimeouts>().cloned() {
            // If retries are exhausted, remove the response headers timeout,
            // since we're not blocked on making a decision on a retry decision.
            // This may give the request additional time to be satisfied.
            if !retries_remain {
                timeouts.response_headers = None;
            }
            dst.insert(timeouts);
        }

        // The HTTP server sets a ClientHandle with the client's address and a means
        // to close the server-side connection.
        if let Some(client_handle) = src.get::<http::ClientHandle>().cloned() {
            dst.insert(client_handle);
        }

        // The legacy response classifier is set for the endpoint stack to use.
        // This informs endpoint-level behavior (failure accrual, etc.).
        // TODO(ver): This should ultimately be eliminated in favor of
        // failure-accrual specific configuration. The endpoint metrics should
        // be migrated, ultimately...
        if let Some(classify) = src.get::<classify::Response>().cloned() {
            dst.insert(classify);
        }
    }

    fn retry(
        &mut self,
        result: Result<&http::Response<TrailersBody>, &Error>,
    ) -> Option<Self::Future> {
        if self.max_retries == 0 {
            tracing::debug!("No retries left");
            return None;
        }

        if !self.is_retryable(result) {
            return None;
        }

        self.max_retries -= 1;

        let delay = if let Some(backoff) = self.backoff.as_mut() {
            backoff.advance()
        } else {
            time::Duration::ZERO
        };
        Some(time::sleep(delay))
    }
}

impl RetryPolicy {
    fn is_retryable(&self, res: Result<&http::Response<TrailersBody>, &Error>) -> bool {
        let rsp = match res {
            Ok(rsp) => rsp,
            Err(error) => {
                let retries_remain = Self::retryable_error(error);
                tracing::debug!(retries_remain, %error);
                return retries_remain;
            }
        };

        if let Some(codes) = self.retryable_grpc_statuses.as_ref() {
            let grpc_status = Self::grpc_status(rsp);
            let retries_remain = grpc_status.map_or(false, |c| codes.contains(c));
            tracing::debug!(retries_remain, grpc.status = ?grpc_status);
            if retries_remain {
                return true;
            }
        }

        if let Some(statuses) = self.retryable_http_statuses.as_ref() {
            let retries_remain = statuses.contains(rsp.status());
            tracing::debug!(retries_remain, http.status = %rsp.status());
            if retries_remain {
                return true;
            }
        }

        false
    }

    fn grpc_status(rsp: &http::Response<TrailersBody>) -> Option<tonic::Code> {
        if let Some(header) = rsp.headers().get("grpc-status") {
            return Some(header.to_str().ok()?.parse::<i32>().ok()?.into());
        }

        let trailer = rsp.body().trailers()?.get("grpc-status")?;
        Some(trailer.to_str().ok()?.parse::<i32>().ok()?.into())
    }

    fn retryable_error(error: &Error) -> bool {
        // While LoadShed errors are not retries_remain, FailFast errors are, since
        // retrying may put us in another backend that is available.
        if is_caused_by::<svc::LoadShedError>(&**error) {
            return false;
        }
        if is_caused_by::<svc::FailFastError>(&**error) {
            return true;
        }

        if matches!(
            cause_ref::<ResponseTimeoutError>(&**error),
            Some(ResponseTimeoutError::Headers(_))
        ) {
            return true;
        }

        // TODO h2 errors
        // TODO connection errors

        false
    }
}
