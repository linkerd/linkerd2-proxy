use super::{extensions, metrics::RouteLabels};
use futures::future::{Either, Ready};
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

pub type NewHttpRetry<N> = retry::NewHttpRetry<RetryPolicy, RouteLabels, N>;

#[derive(Clone, Debug)]
pub struct RetryPolicy {
    pub timeout: Option<time::Duration>,
    pub retryable_http_statuses: Option<policy::http::StatusRanges>,
    pub retryable_grpc_statuses: Option<policy::grpc::Codes>,

    pub max_retries: usize,
    pub max_request_bytes: usize,
    pub backoff: Option<ExponentialBackoff>,
}

pub type RouteRetryMetrics = retry::MetricFamilies<RouteLabels>;

// === impl RetryPolicy ===

impl svc::Param<retry::Params> for RetryPolicy {
    fn param(&self) -> retry::Params {
        retry::Params {
            max_retries: self.max_retries,
            max_request_bytes: self.max_request_bytes,
            backoff: self.backoff,
        }
    }
}

impl retry::Policy for RetryPolicy {
    type Future = Either<time::Sleep, Ready<()>>;

    fn is_retryable(&self, res: Result<&::http::Response<TrailersBody>, &Error>) -> bool {
        let rsp = match res {
            Ok(rsp) => rsp,
            Err(error) => {
                let retryable = Self::retryable_error(error);
                tracing::debug!(retryable, %error);
                return retryable;
            }
        };

        if let Some(codes) = self.retryable_grpc_statuses.as_ref() {
            let grpc_status = Self::grpc_status(rsp);
            let retryable = grpc_status.map_or(false, |c| codes.contains(c));
            tracing::debug!(retryable, grpc.status = ?grpc_status);
            if retryable {
                return true;
            }
        }

        if let Some(statuses) = self.retryable_http_statuses.as_ref() {
            let retryable = statuses.contains(rsp.status());
            tracing::debug!(retryable, http.status = %rsp.status());
            if retryable {
                return true;
            }
        }

        false
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
}

impl RetryPolicy {
    fn grpc_status(rsp: &http::Response<TrailersBody>) -> Option<tonic::Code> {
        if let Some(header) = rsp.headers().get("grpc-status") {
            return Some(header.to_str().ok()?.parse::<i32>().ok()?.into());
        }

        let trailer = rsp.body().trailers()?.get("grpc-status")?;
        Some(trailer.to_str().ok()?.parse::<i32>().ok()?.into())
    }

    fn retryable_error(error: &Error) -> bool {
        // While LoadShed errors are not retryable, FailFast errors are, since
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
