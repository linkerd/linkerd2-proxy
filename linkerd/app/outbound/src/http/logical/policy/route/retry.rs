use futures::future;
use linkerd_app_core::{
    cause_ref,
    config::ExponentialBackoff,
    is_caused_by,
    proxy::http::{stream_timeouts::ResponseTimeoutError, BoxBody, ClientHandle},
    svc::{CloneParam, FailFastError},
    Error, Result,
};
use linkerd_http_retry::{with_trailers::TrailersBody, ReplayBody};
use linkerd_proxy_client_policy as policy;
// use linkerd_proxy_client_policy as policy;
use linkerd_retry as retry;
use tokio::time;

#[derive(Clone, Debug)]
pub struct RetryPolicy {
    pub num_retries: u16,
    pub timeout: Option<time::Duration>,
    pub max_request_bytes: usize,
    pub backoff: ExponentialBackoff,
}

pub type NewHttpRetry<P, N> =
    linkerd_http_retry::NewHttpRetry<CloneParam<linkerd_http_retry::Params>, P, N>;

#[derive(Clone, Debug)]
pub struct HttpPolicy {
    pub statuses: policy::http::StatusRanges,
    pub policy: RetryPolicy,
}

#[derive(Clone, Debug)]
pub struct HttpPolicyOverrides {
    pub max_retries: Option<u16>,
    pub timeout: Option<time::Duration>,
}

#[derive(Clone, Debug)]
pub struct GrpcPolicy {
    pub codes: policy::grpc::Codes,
    pub policy: RetryPolicy,
}

// === impl NewRetryPolicy ===

// impl<T> retry::NewPolicy<super::Http<T>> for NewRetryPolicy {
//     type Policy = HttpPolicy;

//     fn new_policy(&self, _target: &super::Http<T>) -> Option<Self::Policy> {
//         Some(HttpPolicy {
//             statuses: policy::http::StatusRanges::default(),
//             policy: RetryPolicy {
//                 num_retries: 5,
//                 timeout: None,
//                 max_request_bytes: 1024 * 1024,
//                 backoff: ExponentialBackoff::new_unchecked(
//                     std::time::Duration::from_millis(25),
//                     std::time::Duration::from_millis(25 * 10),
//                     0.0,
//                 ),
//             },
//         })
//     }
// }

// impl<T> retry::NewPolicy<super::Grpc<T>> for NewRetryPolicy {
//     type Policy = GrpcPolicy;

//     fn new_policy(&self, _target: &super::Grpc<T>) -> Option<Self::Policy> {
//         Some(GrpcPolicy {
//             codes: policy::grpc::Codes::default(),
//             policy: RetryPolicy {
//                 num_retries: 5,
//                 timeout: None,
//                 max_request_bytes: 1024 * 1024,
//                 backoff: ExponentialBackoff::new_unchecked(
//                     std::time::Duration::from_millis(25),
//                     std::time::Duration::from_millis(25 * 10),
//                     0.0,
//                 ),
//             },
//         })
//     }
// }

fn clone_request<B: Clone>(orig: &http::Request<B>) -> http::Request<B> {
    // Since the body is already wrapped in a ReplayBody, it must not be obviously too large to
    // buffer/clone.
    let mut new = http::Request::new(orig.body().clone());
    *new.method_mut() = orig.method().clone();
    *new.uri_mut() = orig.uri().clone();
    *new.headers_mut() = orig.headers().clone();
    *new.version_mut() = orig.version();

    // The HTTP server sets a ClientHandle with the client's address and a means
    // to close the server-side connection.
    if let Some(client_handle) = orig.extensions().get::<ClientHandle>().cloned() {
        new.extensions_mut().insert(client_handle);
    }

    new
}

// === impl HttpRetryPolicy ===

impl retry::Policy<http::Request<ReplayBody>, http::Response<TrailersBody>, Error> for HttpPolicy {
    type Future = future::Ready<Self>;

    fn clone_request(&self, req: &http::Request<ReplayBody>) -> Option<http::Request<ReplayBody>> {
        Some(clone_request(req))
    }

    fn retry(
        &self,
        req: &http::Request<ReplayBody>,
        result: Result<&http::Response<TrailersBody>, &Error>,
    ) -> Option<Self::Future> {
        // If the request body exceeds the buffer limit, we can't retry
        // it.
        if req.body().is_capped() {
            tracing::debug!("Request body is too large to be retried");
            return None;
        }

        let retryable = match result {
            Ok(rsp) => {
                let retryable = self.retryable_rsp(rsp);
                tracing::debug!(retryable, status = %rsp.status());
                retryable
            }
            Err(error) => {
                let retryable = self.retryable_error(error);
                tracing::debug!(retryable, %error);
                retryable
            }
        };

        if retryable {
            Some(future::ready(self.clone()))
        } else {
            None
        }
    }
}

impl HttpPolicy {
    fn retryable_rsp<B>(&self, rsp: &http::Response<B>) -> bool {
        self.statuses.contains(rsp.status())
    }

    fn retryable_error(&self, e: &Error) -> bool {
        is_caused_by::<ResponseTimeoutError>(&**e) || is_caused_by::<FailFastError>(&**e)
    }
}

// === impl GrpcRetryPolicy ===

impl retry::Policy<http::Request<ReplayBody>, http::Response<TrailersBody>, Error> for GrpcPolicy {
    type Future = future::Ready<Self>;

    fn clone_request(
        &self,
        req: &http::Request<ReplayBody<BoxBody>>,
    ) -> Option<http::Request<ReplayBody<BoxBody>>> {
        Some(clone_request(req))
    }

    fn retry(
        &self,
        req: &http::Request<ReplayBody<BoxBody>>,
        result: Result<&http::Response<TrailersBody<BoxBody>>, &Error>,
    ) -> Option<Self::Future> {
        // If the request body exceeds the buffer limit, we can't retry
        // it.
        if req.body().is_capped() {
            tracing::debug!("Request body is too large to be retried");
            return None;
        }

        let retryable = match result {
            Ok(rsp) => {
                let code = Self::grpc_status(rsp);
                let retryable = code.map(|c| self.codes.contains(c)).unwrap_or(false);
                tracing::debug!(retryable, ?code);
                retryable
            }
            Err(error) => {
                let retryable = self.retryable_error(error);
                tracing::debug!(retryable, %error);
                retryable
            }
        };

        if retryable {
            Some(future::ready(self.clone()))
        } else {
            None
        }
    }
}

impl GrpcPolicy {
    fn grpc_status(rsp: &http::Response<TrailersBody<BoxBody>>) -> Option<tonic::Code> {
        if let Some(header) = rsp.headers().get("grpc-status") {
            return Some(header.to_str().ok()?.parse::<i32>().ok()?.into());
        }

        let trailer = rsp.body().trailers()?.get("grpc-status")?;
        Some(trailer.to_str().ok()?.parse::<i32>().ok()?.into())
    }

    fn retryable_error(&self, e: &Error) -> bool {
        if let Some(e) = cause_ref::<ResponseTimeoutError>(&**e) {
            return matches!(e, ResponseTimeoutError::Headers(_));
        }

        is_caused_by::<FailFastError>(&**e)
    }
}
