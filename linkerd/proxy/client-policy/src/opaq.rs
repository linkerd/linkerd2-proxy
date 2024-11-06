use crate::RoutePolicy;

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Opaque {
    pub policy: Option<Policy>,
}

pub type Policy = RoutePolicy<Filter, ()>;

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct NonIoErrors;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Filter {}

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

    use once_cell::sync::Lazy;
    use std::sync::Arc;

    pub(crate) static NO_FILTERS: Lazy<Arc<[Filter]>> = Lazy::new(|| Arc::new([]));

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidOpaqueRoute {
        #[error("invalid route metadata: {0}")]
        Meta(#[from] InvalidMeta),

        #[error("invalid distribution: {0}")]
        Distribution(#[from] InvalidDistribution),

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

    impl TryFrom<outbound::proxy_protocol::Opaque> for Opaque {
        type Error = InvalidOpaqueRoute;
        fn try_from(proto: outbound::proxy_protocol::Opaque) -> Result<Self, Self::Error> {
            if proto.routes.len() != 1 {
                return Err(InvalidOpaqueRoute::OnlyOneRoute(proto.routes.len()));
            }

            proto
                .routes
                .into_iter()
                .next()
                .ok_or(InvalidOpaqueRoute::OnlyOneRoute(0))?
                .try_into()
        }
    }

    impl TryFrom<outbound::OpaqueRoute> for Opaque {
        type Error = InvalidOpaqueRoute;

        fn try_from(
            outbound::OpaqueRoute {
                metadata,
                rules,
                error: _, // TODO
            }: outbound::OpaqueRoute,
        ) -> Result<Self, Self::Error> {
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

            let policy = rules
                .into_iter()
                .map(|rule| try_rule(&meta, rule))
                .next()
                .ok_or(InvalidOpaqueRoute::OnlyOneRule(0))??;

            Ok(Self {
                policy: Some(policy),
            })
        }
    }

    impl Opaque {
        pub(crate) fn fill_backends(&self, set: &mut BackendSet) {
            for p in &self.policy {
                p.distribution.fill_backends(set);
            }
        }
    }

    fn try_rule(
        meta: &Arc<Meta>,
        opaque_route::Rule { backends }: opaque_route::Rule,
    ) -> Result<Policy, InvalidOpaqueRoute> {
        let distribution = backends
            .ok_or(InvalidOpaqueRoute::Missing("distribution"))?
            .try_into()?;

        Ok(Policy {
            meta: meta.clone(),
            filters: NO_FILTERS.clone(),
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
            opaque_route::RouteBackend {
                backend,
                invalid: _, // TODO
            }: opaque_route::RouteBackend,
        ) -> Result<Self, Self::Error> {
            let backend = backend.ok_or(InvalidBackend::Missing("backend"))?;
            RouteBackend::try_from_proto(backend, std::iter::empty::<()>())
        }
    }

    // Necessary to satisfy `RouteBackend::try_from_proto` type constraints.
    // TODO(eliza): if filters are added to opaque routes, change this to a
    // proper `TryFrom` impl...
    impl From<()> for Filter {
        fn from(_: ()) -> Self {
            unreachable!("no filters can be configured on opaque routes yet")
        }
    }
}
