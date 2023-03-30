use crate::FailureAccrual;
use linkerd_http_route::{grpc, http};
use std::sync::Arc;

pub use linkerd_http_route::grpc::{filter, find, r#match, RouteMatch};

pub type Policy = crate::RoutePolicy<Filter, Codes>;
pub type Route = grpc::Route<Policy>;
pub type Rule = grpc::Rule<Policy>;

// TODO HTTP2 settings
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Grpc {
    pub routes: Arc<[Route]>,

    /// Configures how endpoints accrue observed failures.
    pub failure_accrual: FailureAccrual,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Filter {
    InjectFailure(filter::InjectFailure),
    RequestHeaders(http::filter::ModifyHeader),
    InternalError(&'static str),
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
                failure_policy: Codes::default(),
            },
        }],
    }
}

impl Default for Grpc {
    fn default() -> Self {
        Self {
            routes: Arc::new([]),
            failure_accrual: Default::default(),
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
        proto::{BackendSet, InvalidBackend, InvalidBreaker, InvalidDistribution, InvalidMeta},
        Meta, RouteBackend, RouteDistribution,
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

        #[error("invalid breaker: {0}")]
        Breaker(#[from] InvalidBreaker),
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

    impl TryFrom<outbound::proxy_protocol::Grpc> for Grpc {
        type Error = InvalidGrpcRoute;
        fn try_from(proto: outbound::proxy_protocol::Grpc) -> Result<Self, Self::Error> {
            let routes = proto
                .routes
                .into_iter()
                .map(try_route)
                .collect::<Result<Arc<[_]>, _>>()?;
            let failure_accrual = match proto.breaker {
                Some(accrual) => accrual.try_into()?,
                None => FailureAccrual::None,
            };
            Ok(Self {
                routes,
                failure_accrual,
            })
        }
    }

    impl Grpc {
        pub fn fill_backends(&self, set: &mut BackendSet) {
            for Route { ref rules, .. } in &*self.routes {
                for Rule { ref policy, .. } in rules {
                    policy.distribution.fill_backends(set);
                }
            }
        }
    }

    fn try_route(proto: outbound::GrpcRoute) -> Result<Route, InvalidGrpcRoute> {
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
            .map(|rule| try_rule(&meta, rule))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Route { hosts, rules })
    }

    fn try_rule(
        meta: &Arc<Meta>,
        proto: outbound::grpc_route::Rule,
    ) -> Result<Rule, InvalidGrpcRoute> {
        let outbound::grpc_route::Rule {
            matches,
            backends,
            filters,
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

        Ok(Rule {
            matches,
            policy: Policy {
                meta: meta.clone(),
                filters,
                distribution,
                failure_policy: Codes::default(),
            },
        })
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
            grpc_route::RouteBackend { backend, filters }: grpc_route::RouteBackend,
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
