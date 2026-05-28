use crate::{FailureAccrual, LoadBiasConfig, RetryAfterConfig};
use linkerd_exp_backoff::ExponentialBackoff;
use linkerd_http_route::http;
use std::{ops::RangeInclusive, sync::Arc, time};

pub use linkerd_http_route::http::{filter, find, r#match, RouteMatch};

pub type Policy = crate::RoutePolicy<Filter, RouteParams>;
pub type Route = http::Route<Policy>;
pub type Rule = http::Rule<Policy>;

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct RouteParams {
    pub timeouts: Timeouts,
    pub retry: Option<Retry>,
    pub allow_l5d_request_headers: bool,
    pub export_hostname_labels: bool,
}

// TODO: keepalive settings, etc.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Http1 {
    pub routes: Arc<[Route]>,

    /// Configures how endpoints accrue observed failures.
    pub failure_accrual: FailureAccrual,
    pub load_bias: Option<LoadBiasConfig>,
    pub retry_after: Option<RetryAfterConfig>,
}

// TODO: window sizes, etc
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Http2 {
    pub routes: Arc<[Route]>,

    /// Configures how endpoints accrue observed failures.
    pub failure_accrual: FailureAccrual,
    pub load_bias: Option<LoadBiasConfig>,
    pub retry_after: Option<RetryAfterConfig>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Filter {
    InjectFailure(filter::InjectFailure),
    Redirect(filter::RedirectRequest),
    RequestHeaders(filter::ModifyHeader),
    ResponseHeaders(filter::ModifyHeader),
    InternalError(&'static str),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Retry {
    pub max_retries: u16,
    pub max_request_bytes: usize,
    pub status_ranges: StatusRanges,
    pub timeout: Option<time::Duration>,
    pub backoff: Option<ExponentialBackoff>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StatusRanges(pub Arc<[RangeInclusive<u16>]>);

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Timeouts {
    pub response: Option<time::Duration>,
    pub idle: Option<time::Duration>,
    pub request: Option<time::Duration>,
}

pub fn default(distribution: crate::RouteDistribution<Filter>) -> Route {
    Route {
        hosts: vec![],
        rules: vec![Rule {
            matches: vec![],
            policy: Policy {
                meta: crate::Meta::new_default("default"),
                filters: Arc::new([]),
                distribution,
                params: RouteParams::default(),
            },
        }],
    }
}

// === impl Http1 ===

impl Default for Http1 {
    fn default() -> Self {
        Self {
            routes: Arc::new([]),
            failure_accrual: Default::default(),
            load_bias: None,
            retry_after: None,
        }
    }
}

// === impl Http2 ===

impl Default for Http2 {
    fn default() -> Self {
        Self {
            routes: Arc::new([]),
            failure_accrual: Default::default(),
            load_bias: None,
            retry_after: None,
        }
    }
}

// === impl StatusRanges ===

impl StatusRanges {
    pub fn contains(&self, code: ::http::StatusCode) -> bool {
        self.0.iter().any(|range| range.contains(&code.as_u16()))
    }
}

impl Default for StatusRanges {
    fn default() -> Self {
        use once_cell::sync::Lazy;
        static STATUSES: Lazy<Arc<[RangeInclusive<u16>]>> = Lazy::new(|| Arc::new([500..=599]));
        Self(STATUSES.clone())
    }
}

// === impl Timeouts ===

#[cfg(feature = "proto")]
pub mod proto {
    use super::*;
    use crate::{
        proto::{
            BackendSet, InvalidBackend, InvalidDistribution, InvalidFailureAccrual, InvalidMeta,
        },
        ClientPolicyOverrides, Meta, RouteBackend, RouteDistribution,
    };
    use linkerd2_proxy_api::outbound::{self, http_route};
    use linkerd_http_route::http::{
        filter::{
            inject_failure::proto::InvalidFailureResponse,
            modify_header::proto::InvalidModifyHeader, redirect::proto::InvalidRequestRedirect,
        },
        r#match::{host::proto::InvalidHostMatch, proto::InvalidRouteMatch},
    };

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidHttpRoute {
        #[error("invalid host match: {0}")]
        HostMatch(#[from] InvalidHostMatch),

        #[error("invalid route match: {0}")]
        RouteMatch(#[from] InvalidRouteMatch),

        #[error("invalid route metadata: {0}")]
        Meta(#[from] InvalidMeta),

        #[error("invalid distribution: {0}")]
        Distribution(#[from] InvalidDistribution),

        #[error("invalid filter: {0}")]
        Filter(#[from] InvalidFilter),

        #[error("invalid failure accrual policy: {0}")]
        Breaker(#[from] InvalidFailureAccrual),

        #[error("invalid request timeout: {0}")]
        RequestTimeout(#[from] prost_types::DurationError),

        #[error("missing {0}")]
        Missing(&'static str),

        #[error("{0}")]
        Timeout(#[from] InvalidTimeouts),

        #[error("{0}")]
        Retry(#[from] InvalidRetry),

        #[error("invalid duration for {0}: {1}")]
        InvalidDuration(&'static str, #[source] prost_types::DurationError),
    }

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidRetry {
        #[error("invalid max-retries: {0}")]
        MaxRetries(u32),

        #[error("invalid condition")]
        Condition,

        #[error("invalid timeout: {0}")]
        Timeout(#[from] prost_types::DurationError),

        #[error("invalid backoff: {0}")]
        Backoff(#[from] crate::proto::InvalidBackoff),
    }

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidTimeouts {
        #[error("invalid response timeout: {0}")]
        Response(prost_types::DurationError),
        #[error("invalid idle timeout: {0}")]
        Idle(prost_types::DurationError),
        #[error("invalid request timeout: {0}")]
        Request(prost_types::DurationError),
    }

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidFilter {
        #[error("missing filter kind")]
        Missing,

        #[error("invalid HTTP failure injector: {0}")]
        FailureInjector(#[from] InvalidFailureResponse),

        #[error("invalid HTTP header modifier: {0}")]
        ModifyHeader(#[from] InvalidModifyHeader),

        #[error("invalid HTTP redirect: {0}")]
        Redirect(#[from] InvalidRequestRedirect),
    }

    pub(crate) fn fill_route_backends(rts: &[Route], set: &mut BackendSet) {
        for Route { ref rules, .. } in rts {
            for Rule { ref policy, .. } in rules {
                policy.distribution.fill_backends(set);
            }
        }
    }

    impl Http1 {
        pub fn try_from(
            overrides: ClientPolicyOverrides,
            proto: outbound::proxy_protocol::Http1,
        ) -> Result<Self, InvalidHttpRoute> {
            let routes = proto
                .routes
                .into_iter()
                .map(|p| try_route(overrides, p))
                .collect::<Result<Arc<[_]>, _>>()?;
            Ok(Self {
                routes,
                failure_accrual: proto.failure_accrual.try_into()?,
                load_bias: proto
                    .load_bias
                    .map(crate::proto::try_load_bias_config)
                    .transpose()
                    .map_err(|e| InvalidHttpRoute::InvalidDuration("load_bias", e))?,
                retry_after: proto
                    .retry_after
                    .map(crate::proto::try_retry_after_config)
                    .transpose()
                    .map_err(|e| InvalidHttpRoute::InvalidDuration("retry_after", e))?,
            })
        }
    }

    impl Http2 {
        pub fn try_from(
            overrides: ClientPolicyOverrides,
            proto: outbound::proxy_protocol::Http2,
        ) -> Result<Self, InvalidHttpRoute> {
            let routes = proto
                .routes
                .into_iter()
                .map(|p| try_route(overrides, p))
                .collect::<Result<Arc<[_]>, _>>()?;
            Ok(Self {
                routes,
                failure_accrual: proto.failure_accrual.try_into()?,
                load_bias: proto
                    .load_bias
                    .map(crate::proto::try_load_bias_config)
                    .transpose()
                    .map_err(|e| InvalidHttpRoute::InvalidDuration("load_bias", e))?,
                retry_after: proto
                    .retry_after
                    .map(crate::proto::try_retry_after_config)
                    .transpose()
                    .map_err(|e| InvalidHttpRoute::InvalidDuration("retry_after", e))?,
            })
        }
    }

    fn try_route(
        overrides: ClientPolicyOverrides,
        proto: outbound::HttpRoute,
    ) -> Result<Route, InvalidHttpRoute> {
        let outbound::HttpRoute {
            hosts,
            rules,
            metadata,
        } = proto;
        let meta = Arc::new(
            metadata
                .ok_or(InvalidMeta("missing metadata"))?
                .try_into()?,
        );
        let hosts = hosts
            .into_iter()
            .map(r#match::MatchHost::try_from)
            .collect::<Result<Vec<_>, _>>()?;

        let rules = rules
            .into_iter()
            .map(|rule| try_rule(&meta, overrides, rule))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Route { hosts, rules })
    }

    fn try_rule(
        meta: &Arc<Meta>,
        overrides: ClientPolicyOverrides,
        proto: outbound::http_route::Rule,
    ) -> Result<Rule, InvalidHttpRoute> {
        #[allow(deprecated)]
        let outbound::http_route::Rule {
            matches,
            backends,
            filters,
            timeouts,
            retry,
            allow_l5d_request_headers,
            request_timeout,
        } = proto;

        let matches = matches
            .into_iter()
            .map(r#match::MatchRequest::try_from)
            .collect::<Result<Vec<_>, InvalidRouteMatch>>()?;

        let filters = filters
            .into_iter()
            .map(Filter::try_from)
            .collect::<Result<Arc<[_]>, _>>()?;

        let distribution = backends
            .ok_or(InvalidHttpRoute::Missing("distribution"))?
            .try_into()?;

        let mut params =
            RouteParams::try_from_proto(timeouts, retry, allow_l5d_request_headers, overrides)?;
        let legacy = request_timeout.map(TryInto::try_into).transpose()?;
        params.timeouts.request = params.timeouts.request.or(legacy);

        Ok(Rule {
            matches,
            policy: Policy {
                meta: meta.clone(),
                filters,
                distribution,
                params,
            },
        })
    }

    impl RouteParams {
        fn try_from_proto(
            timeouts: Option<linkerd2_proxy_api::http_route::Timeouts>,
            retry: Option<http_route::Retry>,
            allow_l5d_request_headers: bool,
            overrides: ClientPolicyOverrides,
        ) -> Result<Self, InvalidHttpRoute> {
            Ok(Self {
                retry: retry.map(Retry::try_from).transpose()?,
                timeouts: timeouts
                    .map(Timeouts::try_from)
                    .transpose()?
                    .unwrap_or_default(),
                allow_l5d_request_headers,
                export_hostname_labels: overrides.export_hostname_labels,
            })
        }
    }

    impl TryFrom<linkerd2_proxy_api::http_route::Timeouts> for Timeouts {
        type Error = InvalidTimeouts;
        fn try_from(
            timeouts: linkerd2_proxy_api::http_route::Timeouts,
        ) -> Result<Self, Self::Error> {
            Ok(Self {
                response: timeouts
                    .response
                    .map(time::Duration::try_from)
                    .transpose()
                    .map_err(InvalidTimeouts::Response)?,
                idle: timeouts
                    .idle
                    .map(time::Duration::try_from)
                    .transpose()
                    .map_err(InvalidTimeouts::Response)?,
                request: timeouts
                    .request
                    .map(time::Duration::try_from)
                    .transpose()
                    .map_err(InvalidTimeouts::Request)?,
            })
        }
    }

    impl TryFrom<outbound::http_route::Retry> for Retry {
        type Error = InvalidRetry;
        fn try_from(retry: outbound::http_route::Retry) -> Result<Self, Self::Error> {
            fn range(
                r: outbound::http_route::retry::conditions::StatusRange,
            ) -> Result<RangeInclusive<u16>, InvalidRetry> {
                let Ok(start) = u16::try_from(r.start) else {
                    return Err(InvalidRetry::Condition);
                };
                let Ok(end) = u16::try_from(r.end) else {
                    return Err(InvalidRetry::Condition);
                };
                if start == 0 || end == 0 || end > 599 || start > end {
                    return Err(InvalidRetry::Condition);
                }
                Ok(start..=end)
            }

            let status_ranges = StatusRanges(
                retry
                    .conditions
                    .ok_or(InvalidRetry::Condition)?
                    .status_ranges
                    .into_iter()
                    .map(range)
                    .collect::<Result<_, _>>()?,
            );
            Ok(Self {
                status_ranges,
                max_retries: u16::try_from(retry.max_retries)
                    .map_err(|_| InvalidRetry::MaxRetries(retry.max_retries))?,
                max_request_bytes: retry.max_request_bytes as _,
                backoff: retry.backoff.map(crate::proto::try_backoff).transpose()?,
                timeout: retry.timeout.map(time::Duration::try_from).transpose()?,
            })
        }
    }

    impl TryFrom<http_route::Distribution> for RouteDistribution<Filter> {
        type Error = InvalidDistribution;
        fn try_from(distribution: http_route::Distribution) -> Result<Self, Self::Error> {
            use http_route::{distribution, WeightedRouteBackend};

            Ok(
                match distribution.kind.ok_or(InvalidDistribution::Missing)? {
                    distribution::Kind::Empty(_) => RouteDistribution::Empty,
                    distribution::Kind::RandomAvailable(distribution::RandomAvailable {
                        backends,
                    }) => {
                        let backends = backends
                            .into_iter()
                            .map(|WeightedRouteBackend { weight, backend }| {
                                let backend = backend
                                    .ok_or(InvalidDistribution::MissingBackend)?
                                    .try_into()?;
                                Ok((backend, weight))
                            })
                            .collect::<Result<Arc<[_]>, InvalidDistribution>>()?;
                        if backends.is_empty() {
                            return Err(InvalidDistribution::Empty("RandomAvailable"));
                        }
                        RouteDistribution::RandomAvailable(backends)
                    }
                    distribution::Kind::FirstAvailable(distribution::FirstAvailable {
                        backends,
                    }) => {
                        let backends = backends
                            .into_iter()
                            .map(RouteBackend::try_from)
                            .collect::<Result<Arc<[_]>, InvalidBackend>>()?;
                        if backends.is_empty() {
                            return Err(InvalidDistribution::Empty("FirstAvailable"));
                        }
                        RouteDistribution::FirstAvailable(backends)
                    }
                },
            )
        }
    }

    impl TryFrom<http_route::RouteBackend> for RouteBackend<Filter> {
        type Error = InvalidBackend;
        fn try_from(
            http_route::RouteBackend {
                backend, filters, ..
            }: http_route::RouteBackend,
        ) -> Result<Self, Self::Error> {
            let backend = backend.ok_or(InvalidBackend::Missing("backend"))?;
            RouteBackend::try_from_proto(backend, filters)
        }
    }

    impl TryFrom<http_route::Filter> for Filter {
        type Error = InvalidFilter;

        fn try_from(filter: http_route::Filter) -> Result<Self, Self::Error> {
            use http_route::filter::Kind;

            match filter.kind.ok_or(InvalidFilter::Missing)? {
                Kind::FailureInjector(filter) => Ok(Filter::InjectFailure(filter.try_into()?)),
                Kind::RequestHeaderModifier(filter) => {
                    Ok(Filter::RequestHeaders(filter.try_into()?))
                }
                Kind::ResponseHeaderModifier(filter) => {
                    Ok(Filter::ResponseHeaders(filter.try_into()?))
                }
                Kind::Redirect(filter) => Ok(Filter::Redirect(filter.try_into()?)),
            }
        }
    }
}

#[cfg(all(test, feature = "proto"))]
mod tests {
    use super::*;
    use crate::{LoadBiasConfig, RetryAfterConfig};

    #[test]
    fn http1_default_has_none_load_bias_and_retry_after() {
        let h1 = Http1::default();
        assert_eq!(
            h1.load_bias, None,
            "Http1::default() must produce None for load_bias"
        );
        assert_eq!(
            h1.retry_after, None,
            "Http1::default() must produce None for retry_after"
        );
    }

    #[test]
    fn http2_default_has_none_load_bias_and_retry_after() {
        let h2 = Http2::default();
        assert_eq!(
            h2.load_bias, None,
            "Http2::default() must produce None for load_bias"
        );
        assert_eq!(
            h2.retry_after, None,
            "Http2::default() must produce None for retry_after"
        );
    }

    #[test]
    fn http1_try_from_parses_load_bias_and_retry_after_from_proto() {
        use linkerd2_proxy_api::outbound;

        let proto = outbound::proxy_protocol::Http1 {
            routes: vec![],
            failure_accrual: None,
            load_bias: Some(outbound::LoadBiasConfig {
                enabled: true,
                penalty: Some(prost_types::Duration {
                    seconds: 3,
                    nanos: 0,
                }),
                penalty_decay: Some(prost_types::Duration {
                    seconds: 8,
                    nanos: 0,
                }),
            }),
            retry_after: Some(outbound::RetryAfterConfig {
                max_duration: Some(prost_types::Duration {
                    seconds: 60,
                    nanos: 0,
                }),
            }),
        };

        let overrides = crate::ClientPolicyOverrides {
            export_hostname_labels: false,
        };
        let result = Http1::try_from(overrides, proto).unwrap();

        assert_eq!(
            result.load_bias,
            Some(LoadBiasConfig {
                enabled: true,
                penalty: std::time::Duration::from_secs(3),
                penalty_decay: std::time::Duration::from_secs(8),
            })
        );
        assert_eq!(
            result.retry_after,
            Some(RetryAfterConfig {
                max_duration: std::time::Duration::from_secs(60),
            })
        );
    }

    #[test]
    fn http2_try_from_parses_load_bias_and_retry_after_from_proto() {
        use linkerd2_proxy_api::outbound;

        let proto = outbound::proxy_protocol::Http2 {
            routes: vec![],
            failure_accrual: None,
            load_bias: Some(outbound::LoadBiasConfig {
                enabled: false,
                penalty: Some(prost_types::Duration {
                    seconds: 2,
                    nanos: 0,
                }),
                penalty_decay: Some(prost_types::Duration {
                    seconds: 12,
                    nanos: 0,
                }),
            }),
            retry_after: Some(outbound::RetryAfterConfig {
                max_duration: Some(prost_types::Duration {
                    seconds: 45,
                    nanos: 0,
                }),
            }),
        };

        let overrides = crate::ClientPolicyOverrides {
            export_hostname_labels: false,
        };
        let result = Http2::try_from(overrides, proto).unwrap();

        assert_eq!(
            result.load_bias,
            Some(LoadBiasConfig {
                enabled: false,
                penalty: std::time::Duration::from_secs(2),
                penalty_decay: std::time::Duration::from_secs(12),
            })
        );
        assert_eq!(
            result.retry_after,
            Some(RetryAfterConfig {
                max_duration: std::time::Duration::from_secs(45),
            })
        );
    }

    #[test]
    fn http1_try_from_produces_none_when_load_bias_and_retry_after_absent() {
        use linkerd2_proxy_api::outbound;

        let proto = outbound::proxy_protocol::Http1 {
            routes: vec![],
            failure_accrual: None,
            load_bias: None,
            retry_after: None,
        };

        let overrides = crate::ClientPolicyOverrides {
            export_hostname_labels: false,
        };
        let result = Http1::try_from(overrides, proto).unwrap();

        assert_eq!(result.load_bias, None);
        assert_eq!(result.retry_after, None);
    }

    #[test]
    fn http1_try_from_rejects_invalid_load_bias_duration() {
        use linkerd2_proxy_api::outbound;

        let proto = outbound::proxy_protocol::Http1 {
            routes: vec![],
            failure_accrual: None,
            load_bias: Some(outbound::LoadBiasConfig {
                enabled: true,
                penalty: Some(prost_types::Duration {
                    seconds: -1,
                    nanos: 0,
                }),
                penalty_decay: None,
            }),
            retry_after: None,
        };

        let overrides = crate::ClientPolicyOverrides {
            export_hostname_labels: false,
        };
        let result = Http1::try_from(overrides, proto);
        assert!(
            result.is_err(),
            "invalid load_bias duration must produce an error"
        );
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("load_bias"),
            "error should mention 'load_bias', got: {msg}"
        );
    }

    #[test]
    fn http1_try_from_rejects_invalid_retry_after_duration() {
        use linkerd2_proxy_api::outbound;

        let proto = outbound::proxy_protocol::Http1 {
            routes: vec![],
            failure_accrual: None,
            load_bias: None,
            retry_after: Some(outbound::RetryAfterConfig {
                max_duration: Some(prost_types::Duration {
                    seconds: -10,
                    nanos: 0,
                }),
            }),
        };

        let overrides = crate::ClientPolicyOverrides {
            export_hostname_labels: false,
        };
        let result = Http1::try_from(overrides, proto);
        assert!(
            result.is_err(),
            "invalid retry_after duration must produce an error"
        );
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("retry_after"),
            "error should mention 'retry_after', got: {msg}"
        );
    }

    #[test]
    fn http2_try_from_rejects_invalid_load_bias_duration() {
        use linkerd2_proxy_api::outbound;

        let proto = outbound::proxy_protocol::Http2 {
            routes: vec![],
            failure_accrual: None,
            load_bias: Some(outbound::LoadBiasConfig {
                enabled: true,
                penalty: None,
                penalty_decay: Some(prost_types::Duration {
                    seconds: -1,
                    nanos: 0,
                }),
            }),
            retry_after: None,
        };

        let overrides = crate::ClientPolicyOverrides {
            export_hostname_labels: false,
        };
        let result = Http2::try_from(overrides, proto);
        assert!(
            result.is_err(),
            "invalid load_bias penalty_decay duration must produce an error"
        );
    }

    #[test]
    fn http2_try_from_produces_none_when_load_bias_and_retry_after_absent() {
        use linkerd2_proxy_api::outbound;

        let proto = outbound::proxy_protocol::Http2 {
            routes: vec![],
            failure_accrual: None,
            load_bias: None,
            retry_after: None,
        };

        let overrides = crate::ClientPolicyOverrides {
            export_hostname_labels: false,
        };
        let result = Http2::try_from(overrides, proto).unwrap();

        assert_eq!(result.load_bias, None);
        assert_eq!(result.retry_after, None);
    }

    #[test]
    fn http2_try_from_rejects_invalid_retry_after_duration() {
        use linkerd2_proxy_api::outbound;

        let proto = outbound::proxy_protocol::Http2 {
            routes: vec![],
            failure_accrual: None,
            load_bias: None,
            retry_after: Some(outbound::RetryAfterConfig {
                max_duration: Some(prost_types::Duration {
                    seconds: -10,
                    nanos: 0,
                }),
            }),
        };

        let overrides = crate::ClientPolicyOverrides {
            export_hostname_labels: false,
        };
        let result = Http2::try_from(overrides, proto);
        assert!(
            result.is_err(),
            "invalid retry_after duration must produce an error"
        );
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("retry_after"),
            "error should mention 'retry_after', got: {msg}"
        );
    }
}
