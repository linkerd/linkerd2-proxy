use crate::FailureAccrual;
use linkerd_http_route::http;
use std::{ops::RangeInclusive, sync::Arc};

pub use linkerd_http_route::http::{filter, find, r#match, RouteMatch};

pub type Policy = crate::RoutePolicy<Filter, StatusRanges>;
pub type Route = http::Route<Policy>;
pub type Rule = http::Rule<Policy>;

// TODO: keepalive settings, etc.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Http1 {
    pub routes: Arc<[Route]>,

    /// Configures how endpoints accrue observed failures.
    pub failure_accrual: FailureAccrual,
}

// TODO: window sizes, etc
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Http2 {
    pub routes: Arc<[Route]>,

    /// Configures how endpoints accrue observed failures.
    pub failure_accrual: FailureAccrual,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Filter {
    InjectFailure(filter::InjectFailure),
    Redirect(filter::RedirectRequest),
    RequestHeaders(filter::ModifyHeader),
    InternalError(&'static str),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StatusRanges(pub Arc<[RangeInclusive<u16>]>);

pub fn default(distribution: crate::RouteDistribution<Filter>) -> Route {
    Route {
        hosts: vec![],
        rules: vec![Rule {
            matches: vec![],
            policy: Policy {
                meta: crate::Meta::new_default("default"),
                filters: Arc::new([]),
                distribution,
                failure_policy: StatusRanges::default(),
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
        }
    }
}

// === impl Http2 ===

impl Default for Http2 {
    fn default() -> Self {
        Self {
            routes: Arc::new([]),
            failure_accrual: Default::default(),
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

#[cfg(feature = "proto")]
pub mod proto {
    use super::*;
    use crate::{
        proto::{
            BackendSet, InvalidBackend, InvalidDistribution, InvalidFailureAccrual, InvalidMeta,
        },
        Meta, RouteBackend, RouteDistribution,
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

        #[error("missing {0}")]
        Missing(&'static str),
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

    impl TryFrom<outbound::proxy_protocol::Http1> for Http1 {
        type Error = InvalidHttpRoute;
        fn try_from(proto: outbound::proxy_protocol::Http1) -> Result<Self, Self::Error> {
            let routes = proto
                .routes
                .into_iter()
                .map(try_route)
                .collect::<Result<Arc<[_]>, _>>()?;
            let failure_accrual = match proto.failure_accrual {
                Some(accrual) => accrual.try_into()?,
                None => FailureAccrual::None,
            };
            Ok(Self {
                routes,
                failure_accrual,
            })
        }
    }

    impl TryFrom<outbound::proxy_protocol::Http2> for Http2 {
        type Error = InvalidHttpRoute;
        fn try_from(proto: outbound::proxy_protocol::Http2) -> Result<Self, Self::Error> {
            let routes = proto
                .routes
                .into_iter()
                .map(try_route)
                .collect::<Result<Arc<[_]>, _>>()?;
            let failure_accrual = match proto.failure_accrual {
                Some(accrual) => accrual.try_into()?,
                None => FailureAccrual::None,
            };
            Ok(Self {
                routes,
                failure_accrual,
            })
        }
    }

    fn try_route(proto: outbound::HttpRoute) -> Result<Route, InvalidHttpRoute> {
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
            .map(|rule| try_rule(&meta, rule))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Route { hosts, rules })
    }

    fn try_rule(
        meta: &Arc<Meta>,
        proto: outbound::http_route::Rule,
    ) -> Result<Rule, InvalidHttpRoute> {
        let outbound::http_route::Rule {
            matches,
            backends,
            filters,
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

        Ok(Rule {
            matches,
            policy: Policy {
                meta: meta.clone(),
                filters,
                distribution,
                failure_policy: StatusRanges::default(),
            },
        })
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
            http_route::RouteBackend { backend, filters }: http_route::RouteBackend,
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
                Kind::Redirect(filter) => Ok(Filter::Redirect(filter.try_into()?)),
            }
        }
    }
}
