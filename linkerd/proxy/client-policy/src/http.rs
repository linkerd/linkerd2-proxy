use linkerd_http_route::http;
use std::sync::Arc;

pub use linkerd_http_route::http::{filter, find, r#match, RouteMatch};

pub type Policy = crate::RoutePolicy<Filter>;
pub type Route = http::Route<Policy>;
pub type Rule = http::Rule<Policy>;

// TODO: keepalive settings, etc.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Http1 {
    pub routes: Arc<[Route]>,
}

// TODO: window sizes, etc
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Http2 {
    pub routes: Arc<[Route]>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Filter {
    InjectFailure(filter::InjectFailure),
    Redirect(filter::RedirectRequest),
    RequestHeaders(filter::ModifyHeader),
    InternalError(&'static str),
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
            },
        }],
    }
}

pub(crate) fn is_default(routes: &[Route]) -> bool {
    routes.iter().all(|route| {
        // `Iterator::all` will return `true` on an empty slice, which means
        // that a route with an empty set of rules will be considered
        // "default". Return `false` here if the slice is empty so that a
        // route with no rules is not treated as a default.
        if route.rules.is_empty() {
            return false;
        };

        route.rules.iter().all(|rule| rule.policy.meta.is_default())
    })
}

impl Default for Http1 {
    fn default() -> Self {
        Self {
            routes: Arc::new([]),
        }
    }
}

impl Default for Http2 {
    fn default() -> Self {
        Self {
            routes: Arc::new([]),
        }
    }
}

#[cfg(feature = "proto")]
pub mod proto {
    use super::*;
    use crate::{
        proto::{InvalidDistribution, InvalidMeta},
        Meta, RouteDistribution,
    };
    use linkerd2_proxy_api::outbound;
    use linkerd_http_route::http::{
        filter::inject_failure::proto::InvalidFailureResponse,
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

        #[error("missing {0}")]
        Missing(&'static str),
    }

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidFilter {
        #[error("missing filter kind")]
        Missing,

        #[error("invalid HTTP failure injector: {0}")]
        FailureInjector(#[from] InvalidFailureResponse),
    }

    pub(crate) fn route_backends(rts: &[Route]) -> impl Iterator<Item = &crate::Backend> {
        rts.iter().flat_map(|Route { ref rules, .. }| {
            rules
                .iter()
                .flat_map(|Rule { ref policy, .. }| policy.distribution.backends())
        })
    }

    impl TryFrom<outbound::proxy_protocol::Http1> for Http1 {
        type Error = InvalidHttpRoute;
        fn try_from(proto: outbound::proxy_protocol::Http1) -> Result<Self, Self::Error> {
            let routes = proto
                .http_routes
                .into_iter()
                .map(try_route)
                .collect::<Result<Arc<[_]>, _>>()?;
            Ok(Self { routes })
        }
    }

    impl TryFrom<outbound::proxy_protocol::Http2> for Http2 {
        type Error = InvalidHttpRoute;
        fn try_from(proto: outbound::proxy_protocol::Http2) -> Result<Self, Self::Error> {
            let routes = proto
                .http_routes
                .into_iter()
                .map(try_route)
                .collect::<Result<Arc<[_]>, _>>()?;
            Ok(Self { routes })
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

        let distribution = {
            let backends = backends.ok_or(InvalidHttpRoute::Missing("distribution"))?;
            RouteDistribution::try_from_proto(meta, backends)?
        };

        Ok(Rule {
            matches,
            policy: Policy {
                meta: meta.clone(),
                filters,
                distribution,
            },
        })
    }

    impl TryFrom<outbound::Filter> for Filter {
        type Error = InvalidFilter;

        fn try_from(filter: outbound::Filter) -> Result<Self, Self::Error> {
            use outbound::filter::Kind;

            match filter.kind.ok_or(InvalidFilter::Missing)? {
                Kind::FailureInjector(filter) => Ok(Filter::InjectFailure(filter.try_into()?)),
            }
        }
    }
}
