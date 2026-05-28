use crate::{FailureAccrual, LoadBiasConfig, RetryAfterConfig};
use linkerd_exp_backoff::ExponentialBackoff;
use linkerd_http_route::{grpc, http};
use std::{sync::Arc, time};

pub use linkerd_http_route::grpc::{filter, find, r#match, RouteMatch};

pub type Policy = crate::RoutePolicy<Filter, RouteParams>;
pub type Route = grpc::Route<Policy>;
pub type Rule = grpc::Rule<Policy>;

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct RouteParams {
    pub timeouts: crate::http::Timeouts,
    pub retry: Option<Retry>,
    pub allow_l5d_request_headers: bool,
    pub export_hostname_labels: bool,
}

// TODO HTTP2 settings
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Grpc {
    pub routes: Arc<[Route]>,

    /// Configures how endpoints accrue observed failures.
    // TODO(ver) Move this to backends and scope to endpoints.
    pub failure_accrual: FailureAccrual,
    pub load_bias: Option<LoadBiasConfig>,
    pub retry_after: Option<RetryAfterConfig>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Filter {
    InjectFailure(filter::InjectFailure),
    RequestHeaders(http::filter::ModifyHeader),
    InternalError(&'static str),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Retry {
    pub max_retries: usize,
    pub max_request_bytes: usize,
    pub codes: Codes,
    pub timeout: Option<time::Duration>,
    pub backoff: Option<ExponentialBackoff>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Codes(pub Arc<std::collections::BTreeSet<u16>>);

pub fn default(distribution: crate::RouteDistribution<Filter>) -> Route {
    Route {
        hosts: vec![],
        rules: vec![Rule {
            matches: vec![],
            policy: Policy {
                meta: crate::Meta::new_default("default"),
                filters: Arc::new([]),
                distribution,
                params: Default::default(),
            },
        }],
    }
}

impl Default for Grpc {
    fn default() -> Self {
        Self {
            routes: Arc::new([]),
            failure_accrual: Default::default(),
            load_bias: None,
            retry_after: None,
        }
    }
}

impl Codes {
    pub fn contains(&self, code: tonic::Code) -> bool {
        self.0.contains(&(code as u16))
    }
}

impl Default for Codes {
    fn default() -> Self {
        use once_cell::sync::Lazy;
        static CODES: Lazy<Arc<std::collections::BTreeSet<u16>>> = Lazy::new(|| {
            Arc::new(
                [
                    tonic::Code::DataLoss,
                    tonic::Code::DeadlineExceeded,
                    tonic::Code::Internal,
                    tonic::Code::PermissionDenied,
                    tonic::Code::Unavailable,
                    tonic::Code::Unknown,
                ]
                .into_iter()
                .map(|c| c as u16)
                .collect(),
            )
        });
        Self(CODES.clone())
    }
}

#[cfg(feature = "proto")]
pub mod proto {
    use super::*;
    use crate::{
        proto::{
            BackendSet, InvalidBackend, InvalidDistribution, InvalidFailureAccrual, InvalidMeta,
        },
        ClientPolicyOverrides, Meta, RouteBackend, RouteDistribution,
    };
    use linkerd2_proxy_api::outbound::{self, grpc_route};
    use linkerd_http_route::{
        grpc::{
            filter::inject_failure::proto::InvalidFailureResponse,
            r#match::proto::InvalidRouteMatch,
        },
        http::{
            filter::{
                modify_header::proto::InvalidModifyHeader, redirect::proto::InvalidRequestRedirect,
            },
            r#match::host::{proto::InvalidHostMatch, MatchHost},
        },
    };

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidGrpcRoute {
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

        #[error("missing {0}")]
        Missing(&'static str),

        #[error("invalid failure accrual policy: {0}")]
        Breaker(#[from] InvalidFailureAccrual),

        #[error("{0}")]
        Retry(#[from] InvalidRetry),

        #[error("invalid request timeout: {0}")]
        RequestTimeout(#[from] prost_types::DurationError),

        #[error("{0}")]
        Timeout(#[from] crate::http::proto::InvalidTimeouts),

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

    impl Grpc {
        pub fn try_from(
            overrides: ClientPolicyOverrides,
            proto: outbound::proxy_protocol::Grpc,
        ) -> Result<Self, InvalidGrpcRoute> {
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
                    .map_err(|e| InvalidGrpcRoute::InvalidDuration("load_bias", e))?,
                retry_after: proto
                    .retry_after
                    .map(crate::proto::try_retry_after_config)
                    .transpose()
                    .map_err(|e| InvalidGrpcRoute::InvalidDuration("retry_after", e))?,
            })
        }

        pub fn fill_backends(&self, set: &mut BackendSet) {
            for Route { ref rules, .. } in &*self.routes {
                for Rule { ref policy, .. } in rules {
                    policy.distribution.fill_backends(set);
                }
            }
        }
    }

    fn try_route(
        overrides: ClientPolicyOverrides,
        proto: outbound::GrpcRoute,
    ) -> Result<Route, InvalidGrpcRoute> {
        let outbound::GrpcRoute {
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
            .map(MatchHost::try_from)
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
        proto: outbound::grpc_route::Rule,
    ) -> Result<Rule, InvalidGrpcRoute> {
        #[allow(deprecated)]
        let outbound::grpc_route::Rule {
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
            .map(r#match::MatchRoute::try_from)
            .collect::<Result<Vec<_>, InvalidRouteMatch>>()?;

        let filters = filters
            .into_iter()
            .map(Filter::try_from)
            .collect::<Result<Arc<[_]>, _>>()?;

        let distribution = backends
            .ok_or(InvalidGrpcRoute::Missing("distribution"))?
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
            retry: Option<grpc_route::Retry>,
            allow_l5d_request_headers: bool,
            overrides: ClientPolicyOverrides,
        ) -> Result<Self, InvalidGrpcRoute> {
            Ok(Self {
                retry: retry.map(Retry::try_from).transpose()?,
                timeouts: timeouts
                    .map(crate::http::Timeouts::try_from)
                    .transpose()?
                    .unwrap_or_default(),
                allow_l5d_request_headers,
                export_hostname_labels: overrides.export_hostname_labels,
            })
        }
    }

    impl TryFrom<outbound::grpc_route::Retry> for Retry {
        type Error = InvalidRetry;

        fn try_from(retry: outbound::grpc_route::Retry) -> Result<Self, Self::Error> {
            let cond = retry.conditions.ok_or(InvalidRetry::Condition)?;
            let codes = Codes(Arc::new(
                [
                    cond.cancelled.then_some(tonic::Code::Cancelled as u16),
                    cond.deadine_exceeded
                        .then_some(tonic::Code::DeadlineExceeded as u16),
                    cond.resource_exhausted
                        .then_some(tonic::Code::ResourceExhausted as u16),
                    cond.internal.then_some(tonic::Code::Internal as u16),
                    cond.unavailable.then_some(tonic::Code::Unavailable as u16),
                ]
                .into_iter()
                .flatten()
                .collect(),
            ));

            Ok(Self {
                codes,
                max_retries: retry.max_retries as usize,
                max_request_bytes: retry.max_request_bytes as _,
                backoff: retry.backoff.map(crate::proto::try_backoff).transpose()?,
                timeout: retry.timeout.map(time::Duration::try_from).transpose()?,
            })
        }
    }

    impl TryFrom<grpc_route::Distribution> for RouteDistribution<Filter> {
        type Error = InvalidDistribution;
        fn try_from(distribution: grpc_route::Distribution) -> Result<Self, Self::Error> {
            use grpc_route::{distribution, WeightedRouteBackend};

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

    impl TryFrom<grpc_route::RouteBackend> for RouteBackend<Filter> {
        type Error = InvalidBackend;
        fn try_from(
            grpc_route::RouteBackend {
                backend, filters, ..
            }: grpc_route::RouteBackend,
        ) -> Result<RouteBackend<Filter>, InvalidBackend> {
            let backend = backend.ok_or(InvalidBackend::Missing("backend"))?;
            RouteBackend::try_from_proto(backend, filters)
        }
    }

    impl TryFrom<grpc_route::Filter> for Filter {
        type Error = InvalidFilter;

        fn try_from(filter: grpc_route::Filter) -> Result<Self, Self::Error> {
            use grpc_route::filter::Kind;

            match filter.kind.ok_or(InvalidFilter::Missing)? {
                Kind::FailureInjector(filter) => Ok(Filter::InjectFailure(filter.try_into()?)),
                Kind::RequestHeaderModifier(filter) => {
                    Ok(Filter::RequestHeaders(filter.try_into()?))
                }
            }
        }
    }
}

#[cfg(all(test, feature = "proto"))]
mod tests {
    use super::*;
    use crate::{LoadBiasConfig, RetryAfterConfig};

    #[test]
    fn grpc_default_has_none_load_bias_and_retry_after() {
        let g = Grpc::default();
        assert_eq!(
            g.load_bias, None,
            "Grpc::default() must produce None for load_bias"
        );
        assert_eq!(
            g.retry_after, None,
            "Grpc::default() must produce None for retry_after"
        );
    }

    #[test]
    fn grpc_try_from_parses_load_bias_and_retry_after_from_proto() {
        use linkerd2_proxy_api::outbound;

        let proto = outbound::proxy_protocol::Grpc {
            routes: vec![],
            failure_accrual: None,
            load_bias: Some(outbound::LoadBiasConfig {
                enabled: true,
                penalty: Some(prost_types::Duration {
                    seconds: 4,
                    nanos: 0,
                }),
                penalty_decay: Some(prost_types::Duration {
                    seconds: 9,
                    nanos: 0,
                }),
            }),
            retry_after: Some(outbound::RetryAfterConfig {
                max_duration: Some(prost_types::Duration {
                    seconds: 90,
                    nanos: 0,
                }),
            }),
        };

        let overrides = crate::ClientPolicyOverrides {
            export_hostname_labels: false,
        };
        let result = Grpc::try_from(overrides, proto).unwrap();

        assert_eq!(
            result.load_bias,
            Some(LoadBiasConfig {
                enabled: true,
                penalty: std::time::Duration::from_secs(4),
                penalty_decay: std::time::Duration::from_secs(9),
            })
        );
        assert_eq!(
            result.retry_after,
            Some(RetryAfterConfig {
                max_duration: std::time::Duration::from_secs(90),
            })
        );
    }

    #[test]
    fn grpc_try_from_rejects_invalid_load_bias_duration() {
        use linkerd2_proxy_api::outbound;

        let proto = outbound::proxy_protocol::Grpc {
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
        let result = Grpc::try_from(overrides, proto);
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
    fn grpc_try_from_rejects_invalid_retry_after_duration() {
        use linkerd2_proxy_api::outbound;

        let proto = outbound::proxy_protocol::Grpc {
            routes: vec![],
            failure_accrual: None,
            load_bias: None,
            retry_after: Some(outbound::RetryAfterConfig {
                max_duration: Some(prost_types::Duration {
                    seconds: -1,
                    nanos: 0,
                }),
            }),
        };

        let overrides = crate::ClientPolicyOverrides {
            export_hostname_labels: false,
        };
        let result = Grpc::try_from(overrides, proto);
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
