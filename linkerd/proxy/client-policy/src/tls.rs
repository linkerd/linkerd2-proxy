use linkerd_tls_route as tls;
use std::sync::Arc;

pub use linkerd_tls_route::{find, sni, RouteMatch};

pub type Policy = crate::RoutePolicy<Filter, ()>;
pub type Route = tls::Route<Policy>;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Tls {
    pub routes: Arc<[Route]>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Filter {}

pub fn default(distribution: crate::RouteDistribution<Filter>) -> Route {
    Route {
        snis: vec![],
        policy: Policy {
            meta: crate::Meta::new_default("default"),
            filters: Arc::new([]),
            params: (),
            distribution,
        },
    }
}

impl Default for Tls {
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
        proto::{BackendSet, InvalidBackend, InvalidDistribution, InvalidMeta},
        Meta, RouteBackend, RouteDistribution,
    };
    use linkerd2_proxy_api::outbound::{self, tls_route};
    use linkerd_tls_route::sni::proto::InvalidSniMatch;

    use once_cell::sync::Lazy;
    use std::sync::Arc;

    pub(crate) static NO_FILTERS: Lazy<Arc<[Filter]>> = Lazy::new(|| Arc::new([]));

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidTlsRoute {
        #[error("invalid sni match: {0}")]
        SniMatch(#[from] InvalidSniMatch),

        #[error("invalid route metadata: {0}")]
        Meta(#[from] InvalidMeta),

        #[error("invalid distribution: {0}")]
        Distribution(#[from] InvalidDistribution),

        /// Note: this restriction may be removed in the future, if a way of
        /// actually matching rules for tls routes is added.
        #[error("a tls route must have exactly one rule, but {0} were provided")]
        OnlyOneRule(usize),

        #[error("missing {0}")]
        Missing(&'static str),
    }

    pub(crate) fn fill_route_backends(rts: &[Route], set: &mut BackendSet) {
        for Route { ref policy, .. } in rts {
            policy.distribution.fill_backends(set);
        }
    }

    impl TryFrom<outbound::proxy_protocol::Tls> for Tls {
        type Error = InvalidTlsRoute;
        fn try_from(proto: outbound::proxy_protocol::Tls) -> Result<Self, Self::Error> {
            let routes = proto
                .routes
                .into_iter()
                .map(try_route)
                .collect::<Result<Arc<[_]>, _>>()?;
            Ok(Self { routes })
        }
    }

    fn try_route(proto: outbound::TlsRoute) -> Result<Route, InvalidTlsRoute> {
        let outbound::TlsRoute {
            snis,
            rules,
            metadata,
        } = proto;
        let meta = Arc::new(
            metadata
                .ok_or(InvalidMeta("missing metadata"))?
                .try_into()?,
        );
        let snis = snis
            .into_iter()
            .map(sni::MatchSni::try_from)
            .collect::<Result<Vec<_>, _>>()?;

        // Currently, TLS rules have no match expressions, so if there's
        // more than one rule, we have no way of determining which one to
        // use. Therefore, require that there's exactly one rule.
        if rules.len() != 1 {
            return Err(InvalidTlsRoute::OnlyOneRule(rules.len()));
        }

        let policy = rules
            .into_iter()
            .map(|rule| try_rule(&meta, rule))
            .next()
            .ok_or(InvalidTlsRoute::OnlyOneRule(0))??;

        Ok(Route { snis, policy })
    }

    fn try_rule(
        meta: &Arc<Meta>,
        outbound::tls_route::Rule { backends, .. }: outbound::tls_route::Rule,
    ) -> Result<Policy, InvalidTlsRoute> {
        let distribution = backends
            .ok_or(InvalidTlsRoute::Missing("distribution"))?
            .try_into()?;

        Ok(Policy {
            meta: meta.clone(),
            filters: NO_FILTERS.clone(),
            distribution,
            params: (),
        })
    }

    impl TryFrom<tls_route::Distribution> for RouteDistribution<Filter> {
        type Error = InvalidDistribution;
        fn try_from(distribution: tls_route::Distribution) -> Result<Self, Self::Error> {
            use tls_route::{distribution, WeightedRouteBackend};

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

    impl TryFrom<tls_route::RouteBackend> for RouteBackend<Filter> {
        type Error = InvalidBackend;
        fn try_from(
            tls_route::RouteBackend { backend }: tls_route::RouteBackend,
        ) -> Result<Self, Self::Error> {
            let backend = backend.ok_or(InvalidBackend::Missing("backend"))?;
            RouteBackend::try_from_proto(backend, std::iter::empty::<()>())
        }
    }

    // Necessary to satisfy `RouteBackend::try_from_proto` type constraints.
    // TODO(eliza): if filters are added to tls routes, change this to a
    // proper `TryFrom` impl...
    impl From<()> for Filter {
        fn from(_: ()) -> Self {
            unreachable!("no filters can be configured on tls routes yet")
        }
    }
}
