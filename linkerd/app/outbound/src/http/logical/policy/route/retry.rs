use futures::future;
use linkerd_app_core::{
    cause_ref,
    config::ExponentialBackoff,
    is_caused_by,
    proxy::http::{stream_timeouts::ResponseTimeoutError, ClientHandle},
    svc, Error, Result,
};
use linkerd_http_retry::{self as retry, with_trailers::TrailersBody, ReplayBody};
use linkerd_proxy_client_policy as policy;
// use std::sync::Arc;
use tokio::time;

#[derive(Clone, Debug)]
pub struct RetryPolicy {
    pub num_retries: u16,
    pub timeout: Option<time::Duration>,
    pub max_request_bytes: usize,
    pub backoff: ExponentialBackoff,
    pub retryable_http_statuses: Option<policy::http::StatusRanges>,
    pub retryable_grpc_statuses: Option<policy::grpc::Codes>,
}

pub type NewHttpRetry<P, N> = retry::NewHttpRetry<P, N>;

impl svc::Param<retry::Params> for RetryPolicy {
    fn param(&self) -> retry::Params {
        retry::Params {
            max_request_bytes: self.max_request_bytes,
        }
    }
}

// === impl RetryPolicy ===

impl retry::Policy<http::Request<ReplayBody>, http::Response<TrailersBody>, Error> for RetryPolicy {
    type Future = future::BoxFuture<'static, Self>;

    fn clone_request(&self, orig: &http::Request<ReplayBody>) -> Option<http::Request<ReplayBody>> {
        // Since the body is already wrapped in a ReplayBody, it must not be obviously too large to
        // buffer/clone.
        let mut new = http::Request::new(orig.body().clone());
        *new.method_mut() = orig.method().clone();
        *new.uri_mut() = orig.uri().clone();
        *new.headers_mut() = orig.headers().clone();
        *new.version_mut() = orig.version();

        // TODO handle other extensions like stream timeouts
        // TODO add a marker extension to indicate that the request has been retried.

        // The HTTP server sets a ClientHandle with the client's address and a means
        // to close the server-side connection.
        if let Some(client_handle) = orig.extensions().get::<ClientHandle>().cloned() {
            new.extensions_mut().insert(client_handle);
        }

        Some(new)
    }

    fn retry(
        &self,
        req: &http::Request<ReplayBody>,
        result: Result<&http::Response<TrailersBody>, &Error>,
    ) -> Option<Self::Future> {
        if self.num_retries == 0 {
            tracing::debug!("No retries left");
            return None;
        }

        // If the request body exceeds the buffer limit, we can't retry
        // it.
        if req.body().is_capped() {
            tracing::debug!("Request body is too large to be retried");
            return None;
        }

        let retryable = match result {
            Ok(rsp) => self.is_retryable(rsp),
            Err(error) => {
                let retryable = retryable_error(error);
                tracing::debug!(retryable, %error);
                retryable
            }
        };

        if !retryable {
            return None;
        }

        let (delay, backoff) = self.backoff.next();

        let mut next = self.clone();
        next.backoff = backoff;
        next.num_retries -= 1;

        Some(Box::pin(async move {
            time::sleep(delay).await;
            next
        }))
    }
}

impl RetryPolicy {
    fn is_retryable(&self, rsp: &http::Response<TrailersBody>) -> bool {
        if let Some(codes) = self.retryable_grpc_statuses.as_ref() {
            let grpc_status = grpc_status(rsp);
            let retryable = grpc_status.map_or(false, |c| codes.contains(c));
            tracing::debug!(retryable, grpc.status = ?grpc_status);
            if retryable {
                return true;
            }
        }

        if let Some(statuses) = self.retryable_http_statuses.as_ref() {
            let retryable = statuses.contains(rsp.status());
            tracing::debug!(retryable, http.status = %rsp.status());
            retryable
        } else {
            false
        }
    }
}

fn grpc_status(rsp: &http::Response<TrailersBody>) -> Option<tonic::Code> {
    if let Some(header) = rsp.headers().get("grpc-status") {
        return Some(header.to_str().ok()?.parse::<i32>().ok()?.into());
    }

    let trailer = rsp.body().trailers()?.get("grpc-status")?;
    Some(trailer.to_str().ok()?.parse::<i32>().ok()?.into())
}

fn retryable_error(e: &Error) -> bool {
    if let Some(e) = cause_ref::<ResponseTimeoutError>(&**e) {
        return matches!(e, ResponseTimeoutError::Headers(_));
    }

    // While LoadShed errors are not retryable, FailFast errors are, since
    // retrying may put us in another backend that is available.
    is_caused_by::<svc::FailFastError>(&**e)
}
