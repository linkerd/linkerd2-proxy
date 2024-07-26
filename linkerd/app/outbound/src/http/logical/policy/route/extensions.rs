use super::retry::RetryPolicy;
use linkerd_app_core::{config::ExponentialBackoff, proxy::http, svc};
use linkerd_proxy_client_policy as policy;
use std::task::{Context, Poll};
use tokio::time;

#[derive(Clone, Debug)]
pub struct Params {
    pub retry: Option<RetryPolicy>,
    pub timeouts: policy::http::Timeouts,
    pub allow_l5d_request_headers: bool,
}

// A request extension that marks the number of times a request has been
// attempted.
#[derive(Clone, Debug)]
pub struct Attempt(pub std::num::NonZeroU16);

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
    T: svc::Param<Params>,
    N: svc::NewService<T>,
{
    type Service = SetExtensions<N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let params = target.param();
        let inner = self.inner.new_service(target);
        SetExtensions { params, inner }
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
        let retry = self.configure_retry(req.headers_mut());

        // Ensure that we get response headers within the retry timeout. Note
        // that this may be cleared super::retry::RetryPolicy::set_extensions.
        let mut timeouts = self.configure_timeouts(req.headers_mut());
        timeouts.response_headers = retry.as_ref().and_then(|r| r.timeout);

        tracing::debug!(?retry, ?timeouts, "Initializing route extensions");
        if let Some(retry) = retry {
            let _prior = req.extensions_mut().insert(retry);
            debug_assert!(_prior.is_none(), "RetryPolicy must only be configured once");
        }

        let _prior = req.extensions_mut().insert(timeouts);
        debug_assert!(
            _prior.is_none(),
            "StreamTimeouts must only be configured once"
        );

        let _prior = req.extensions_mut().insert(Attempt(1.try_into().unwrap()));
        debug_assert!(_prior.is_none(), "Attempts must only be configured once");

        self.inner.call(req)
    }
}

impl<S> SetExtensions<S> {
    fn configure_retry(&self, req: &mut http::HeaderMap) -> Option<RetryPolicy> {
        if !self.params.allow_l5d_request_headers {
            return self.params.retry.clone();
        }

        let user_retry_http = req
            .remove("l5d-retry-http")
            .and_then(|val| val.to_str().ok().and_then(parse_http_conditions));
        let user_retry_grpc = req
            .remove("l5d-retry-grpc")
            .and_then(|val| val.to_str().ok().and_then(parse_grpc_conditions));
        let user_retry_limit = req
            .remove("l5d-retry-limit")
            .and_then(|val| val.to_str().ok().and_then(|v| v.parse::<usize>().ok()));
        let user_retry_timeout = req.remove("l5d-retry-timeout").and_then(parse_duration);

        if let Some(retry) = self.params.retry.clone() {
            return Some(RetryPolicy {
                timeout: user_retry_timeout.or(retry.timeout),
                retryable_http_statuses: user_retry_http.or(retry.retryable_http_statuses.clone()),
                retryable_grpc_statuses: user_retry_grpc.or(retry.retryable_grpc_statuses.clone()),
                max_retries: user_retry_limit.unwrap_or(retry.max_retries),
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
                    max_retries: retry_limit.unwrap_or(1),
                    max_request_bytes: 64 * 1024,
                    backoff: Some(ExponentialBackoff::new_unchecked(
                        std::time::Duration::from_millis(25),
                        std::time::Duration::from_millis(250),
                        1.0,
                    )),
                })
            }
        }
    }

    fn configure_timeouts(&self, req: &mut http::HeaderMap) -> http::StreamTimeouts {
        let mut timeouts = http::StreamTimeouts {
            response_headers: None,
            response_end: self.params.timeouts.response,
            idle: self.params.timeouts.idle,
            limit: self.params.timeouts.request.map(Into::into),
        };

        if !self.params.allow_l5d_request_headers {
            return timeouts;
        }

        // Accept both a shorthand and longer, more explicit version, the
        // latter taking precedence.
        if let Some(t) = req.remove("l5d-timeout").and_then(parse_duration) {
            timeouts.limit = Some(t.into());
        }
        if let Some(t) = req.remove("l5d-request-timeout").and_then(parse_duration) {
            timeouts.limit = Some(t.into());
        }

        if let Some(t) = req.remove("l5d-response-timeout").and_then(parse_duration) {
            timeouts.response_end = Some(t);
        }

        timeouts
    }
}

fn parse_http_conditions(s: &str) -> Option<policy::http::StatusRanges> {
    fn to_code(s: &str) -> Option<u16> {
        let code = s.parse::<u16>().ok()?;
        if (100..600).contains(&code) {
            Some(code)
        } else {
            None
        }
    }

    Some(policy::http::StatusRanges(
        s.split(',')
            .filter_map(|cond| {
                if cond.eq_ignore_ascii_case("5xx") {
                    return Some(500..=599);
                }
                if cond.eq_ignore_ascii_case("gateway-error") {
                    return Some(502..=504);
                }

                if let Some(code) = to_code(cond) {
                    return Some(code..=code);
                }
                if let Some((start, end)) = cond.split_once('-') {
                    if let (Some(s), Some(e)) = (to_code(start), to_code(end)) {
                        if s <= e {
                            return Some(s..=e);
                        }
                    }
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

// Copied from the policy controller so that we handle the same duration values
// as we do in the YAML config.
fn parse_duration(hv: http::HeaderValue) -> Option<time::Duration> {
    #[inline]
    fn parse(s: &str) -> Option<time::Duration> {
        let s = s.trim();
        let offset = s.rfind(|c: char| c.is_ascii_digit())?;
        let (magnitude, unit) = s.split_at(offset + 1);
        let magnitude = magnitude.parse::<u64>().ok()?;

        let mul = match unit {
            "" if magnitude == 0 => 0,
            "ms" => 1,
            "s" => 1000,
            "m" => 1000 * 60,
            "h" => 1000 * 60 * 60,
            "d" => 1000 * 60 * 60 * 24,
            _ => return None,
        };

        let ms = magnitude.checked_mul(mul)?;
        Some(time::Duration::from_millis(ms))
    }
    let s = hv.to_str().ok()?;
    let Some(d) = parse(s) else {
        tracing::debug!("Invalid duration: {:?}", s);
        return None;
    };
    Some(d)
}
