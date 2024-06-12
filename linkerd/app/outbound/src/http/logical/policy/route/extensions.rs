use super::retry::RetryPolicy;
use linkerd_app_core::{classify, config::ExponentialBackoff, proxy::http, svc};
use linkerd_proxy_client_policy as policy;
use std::task::{Context, Poll};

#[derive(Clone, Debug)]
pub struct Params {
    pub retry: Option<RetryPolicy>,
    pub timeouts: policy::http::Timeouts,
}

#[derive(Clone, Debug)]
pub struct NewSetExtensions<N> {
    inner: N,
}

#[derive(Clone, Debug)]
pub struct SetExtensions<S> {
    inner: S,
    params: Params,
}

// === impl NewSetExtensions ===

impl<N> NewSetExtensions<N> {
    pub fn layer() -> impl svc::Layer<N, Service = Self> + Clone {
        svc::layer::mk(|inner| Self { inner })
    }
}

impl<T, N> svc::NewService<T> for NewSetExtensions<N>
where
    N: svc::NewService<T>,
    T: svc::Param<Params>,
{
    type Service = SetExtensions<N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let params = target.param();
        let inner = self.inner.new_service(target);
        SetExtensions { params, inner }
    }
}

pub(super) fn clone_request(orig: &::http::Extensions, new: &mut ::http::Extensions) {
    if let Some(retry) = orig.get::<RetryPolicy>() {
        new.insert(retry.clone());
    }

    if let Some(timeouts) = orig.get::<http::StreamTimeouts>() {
        new.insert(timeouts.clone());
    }

    // The HTTP server sets a ClientHandle with the client's address and a means
    // to close the server-side connection.
    if let Some(client_handle) = orig.get::<http::ClientHandle>().cloned() {
        new.insert(client_handle);
    }

    if let Some(classify) = orig.get::<classify::Response>().cloned() {
        new.insert(classify);
    }
}

// === impl SetExtensions ===

impl<B, S> svc::Service<http::Request<B>> for SetExtensions<S>
where
    S: svc::Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        let retry = configure_retry(req.headers(), self.params.retry.clone());

        let timeouts = configure_timeouts(
            req.headers(),
            self.params.timeouts.clone(),
            retry.as_ref().and_then(|r| r.timeout),
        );
        tracing::debug!(?retry, ?timeouts, "Setting extensions");

        if let Some(retry) = retry {
            let _prior = req.extensions_mut().insert(retry);
            debug_assert!(_prior.is_none(), "RetryPolicy must only be configured once");
        }

        let _prior = req.extensions_mut().insert(timeouts);
        debug_assert!(
            _prior.is_none(),
            "StreamTimeouts must only be configured once"
        );

        self.inner.call(req)
    }
}

fn parse_http_conditions(s: &str) -> Option<policy::http::StatusRanges> {
    Some(policy::http::StatusRanges(
        s.split(',')
            .filter_map(|cond| {
                if cond.eq_ignore_ascii_case("5xx") {
                    return Some(500..=599);
                }
                if cond.eq_ignore_ascii_case("gateway-error") {
                    return Some(502..=504);
                }

                None
            })
            .collect(),
    ))
}

fn parse_grpc_conditions(s: &str) -> Option<policy::grpc::Codes> {
    Some(policy::grpc::Codes(std::sync::Arc::new(
        s.split(',')
            .filter_map(|cond| {
                if cond.eq_ignore_ascii_case("cancelled") {
                    return Some(tonic::Code::Cancelled as u16);
                }
                if cond.eq_ignore_ascii_case("deadline-exceeded") {
                    return Some(tonic::Code::DeadlineExceeded as u16);
                }
                if cond.eq_ignore_ascii_case("internal") {
                    return Some(tonic::Code::Internal as u16);
                }
                if cond.eq_ignore_ascii_case("resource-exhausted") {
                    return Some(tonic::Code::ResourceExhausted as u16);
                }
                if cond.eq_ignore_ascii_case("unavailable") {
                    return Some(tonic::Code::Unavailable as u16);
                }
                None
            })
            .collect(),
    )))
}

fn configure_retry(req: &http::HeaderMap, orig: Option<RetryPolicy>) -> Option<RetryPolicy> {
    let user_retry_http = req
        .get("l5d-retry-http")
        .and_then(|val| val.to_str().ok().and_then(parse_http_conditions));
    let user_retry_grpc = req
        .get("l5d-retry-grpc")
        .and_then(|val| val.to_str().ok().and_then(parse_grpc_conditions));
    let user_retry_limit = req
        .get("l5d-retry-limit")
        .and_then(|val| val.to_str().ok().and_then(|v| v.parse::<u16>().ok()));
    let user_retry_timeout = req.get("l5d-retry-timeout").and_then(|val| {
        val.to_str()
            .ok()
            .and_then(|val| val.parse().ok().map(std::time::Duration::from_secs_f64))
    });

    if let Some(retry) = orig {
        return Some(RetryPolicy {
            timeout: user_retry_timeout.or(retry.timeout),
            num_retries: user_retry_limit.unwrap_or(retry.num_retries),
            retryable_http_statuses: user_retry_http.or(retry.retryable_http_statuses.clone()),
            retryable_grpc_statuses: user_retry_grpc.or(retry.retryable_grpc_statuses.clone()),
            ..retry
        });
    }

    match (
        user_retry_http,
        user_retry_grpc,
        user_retry_limit,
        user_retry_timeout,
    ) {
        (None, None, None, None) => None,
        (retryable_http_statuses, retryable_grpc_statuses, retry_limit, timeout) => {
            Some(RetryPolicy {
                timeout,
                retryable_http_statuses,
                retryable_grpc_statuses,
                num_retries: retry_limit.unwrap_or(3),
                max_request_bytes: 64 * 1024,
                backoff: ExponentialBackoff::new_unchecked(
                    std::time::Duration::from_millis(25),
                    std::time::Duration::from_millis(250),
                    0.0,
                ),
            })
        }
    }
}

fn configure_timeouts(
    req: &http::HeaderMap,
    orig: policy::http::Timeouts,
    retry: Option<std::time::Duration>,
) -> http::StreamTimeouts {
    let user_timeout = req.get("l5d-timeout").and_then(|val| {
        val.to_str()
            .ok()
            .and_then(|val| val.parse().ok().map(std::time::Duration::from_secs_f64))
    });
    let timeout = user_timeout.or(orig.stream);

    let user_response_timeout = req.get("l5d-response-timeout").and_then(|val| {
        val.to_str()
            .ok()
            .and_then(|val| val.parse().ok().map(std::time::Duration::from_secs_f64))
    });
    let response_timeout = user_response_timeout.or(orig.response).or(timeout);

    http::StreamTimeouts {
        response_headers: retry.or(response_timeout),
        response_end: response_timeout,
        idle: orig.idle,
        limit: timeout.map(Into::into),
    }
}
