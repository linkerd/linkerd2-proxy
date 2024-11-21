use linkerd_opaq_route as opaq;

pub type Policy = crate::RoutePolicy<Filter, ()>;
pub type Route = opaq::Route<Policy>;
pub type Rule = opaq::Rule<Policy>;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Opaque {
    pub routes: Option<Route>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct NonIoErrors;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Filter {
    ForbiddenRoute,
    InvalidBackend(std::sync::Arc<str>),
    InternalError(&'static str),
}

impl NonIoErrors {
    pub fn contains(&self, e: &(dyn std::error::Error + 'static)) -> bool {
        // Naively assume that all non-I/O errors are failures.
        !linkerd_error::is_caused_by::<std::io::Error>(e)
    }
}

#[cfg(feature = "proto")]
pub(crate) mod proto {
    use super::*;
    use crate::{
        proto::{BackendSet, InvalidBackend, InvalidDistribution, InvalidMeta},
        Meta, RouteBackend, RouteDistribution,
    };
    use linkerd2_proxy_api::outbound::{self, opaque_route};
    use std::sync::Arc;

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidOpaqueRoute {
        #[error("invalid route metadata: {0}")]
        Meta(#[from] InvalidMeta),

        #[error("invalid distribution: {0}")]
        Distribution(#[from] InvalidDistribution),

        #[error("invalid filter: {0}")]
        Filter(#[from] InvalidFilter),

        /// Note: this restriction may be removed in the future, if a way of
        /// actually matching rules for opaque routes is added.
        #[error("an opaque route must have exactly one rule, but {0} were provided")]
        OnlyOneRule(usize),

        /// Note: this restriction may be removed in the future, if a way of
        /// actually matching rules for opaque routes is added.
        #[error("a `ProxyProtocol::Opaque` must have exactly one route, but {0} were provided")]
        OnlyOneRoute(usize),

        #[error("no filters can be configured on opaque routes yet")]
        NoFilters,

        #[error("missing {0}")]
        Missing(&'static str),
    }

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidFilter {
        #[error("invalid route error kind: {0}")]
        InvalidRouteErrorKind(i32),

        #[error("missing filter kind")]
        Missing,
    }

    pub(crate) fn fill_route_backends(rts: Option<&Route>, set: &mut BackendSet) {
        if let Some(Route { policy, .. }) = rts {
            policy.distribution.fill_backends(set);
        }
    }

    impl TryFrom<outbound::proxy_protocol::Opaque> for Opaque {
        type Error = InvalidOpaqueRoute;
        fn try_from(proto: outbound::proxy_protocol::Opaque) -> Result<Self, Self::Error> {
            if proto.routes.len() > 1 {
                return Err(InvalidOpaqueRoute::OnlyOneRoute(proto.routes.len()));
            }
            let routes = proto.routes.into_iter().next().map(try_route).transpose()?;

            Ok(Self { routes })
        }
    }

    fn try_route(
        outbound::OpaqueRoute { metadata, rules }: outbound::OpaqueRoute,
    ) -> Result<Route, InvalidOpaqueRoute> {
        let meta = Arc::new(
            metadata
                .ok_or(InvalidMeta("missing metadata"))?
                .try_into()?,
        );

        // Currently, opaque rules have no match expressions, so if there's
        // more than one rule, we have no way of determining which one to
        // use. Therefore, require that there's exactly one rule.
        if rules.len() != 1 {
            return Err(InvalidOpaqueRoute::OnlyOneRule(rules.len()));
        }

        let rule = rules.first().cloned().expect("already checked");
        let policy = try_rule(&meta, rule)?;
        Ok(Route { policy })
    }

    fn try_rule(
        meta: &Arc<Meta>,
        opaque_route::Rule { backends, filters }: opaque_route::Rule,
    ) -> Result<Policy, InvalidOpaqueRoute> {
        let distribution = backends
            .ok_or(InvalidOpaqueRoute::Missing("distribution"))?
            .try_into()?;

        let filters = filters
            .into_iter()
            .map(Filter::try_from)
            .collect::<Result<Arc<[_]>, _>>()?;

        Ok(Policy {
            meta: meta.clone(),
            filters,
            params: (),
            distribution,
        })
    }

    impl TryFrom<opaque_route::Distribution> for RouteDistribution<Filter> {
        type Error = InvalidDistribution;
        fn try_from(distribution: opaque_route::Distribution) -> Result<Self, Self::Error> {
            use opaque_route::{distribution, WeightedRouteBackend};

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

    impl TryFrom<opaque_route::RouteBackend> for RouteBackend<Filter> {
        type Error = InvalidBackend;
        fn try_from(
            opaque_route::RouteBackend { backend, filters }: opaque_route::RouteBackend,
        ) -> Result<RouteBackend<Filter>, InvalidBackend> {
            let backend = backend.ok_or(InvalidBackend::Missing("backend"))?;
            RouteBackend::try_from_proto(backend, filters)
        }
    }

    impl TryFrom<opaque_route::Filter> for Filter {
        type Error = InvalidFilter;

        fn try_from(filter: opaque_route::Filter) -> Result<Self, Self::Error> {
            use linkerd2_proxy_api::opaque_route::{route_error, InvalidBackendError, RouteError};
            use opaque_route::filter::Kind;

            match filter.kind.ok_or(InvalidFilter::Missing)? {
                Kind::InvalidBackendError(InvalidBackendError { message }) => {
                    Ok(Filter::InvalidBackend(message.into()))
                }
                Kind::RouteError(RouteError { kind })
                    if kind == route_error::Kind::Forbidden.into() =>
                {
                    Ok(Filter::ForbiddenRoute)
                }

                Kind::RouteError(RouteError { kind }) => {
                    Err(InvalidFilter::InvalidRouteErrorKind(kind))
                }
            }
        }
    }
}
