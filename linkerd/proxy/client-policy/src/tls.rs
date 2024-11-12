use linkerd_tls_route as tls;
pub use linkerd_tls_route::{find, sni, RouteMatch};
use std::sync::Arc;

pub type Policy = crate::RoutePolicy<Filter, ()>;
pub type Route = tls::Route<Policy>;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Tls {
    pub routes: Arc<[Route]>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Filter {
    ForbiddenRoute,
    InvalidBackend(Arc<str>),
    InternalError(&'static str),
}

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
pub(crate) mod proto {
    use super::*;
    use crate::{
        proto::{BackendSet, InvalidBackend, InvalidDistribution, InvalidMeta},
        Backend, Meta, RouteBackend, RouteDistribution,
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
        /// actually matching rules for TLS routes is added.
        #[error("a TLS route must have exactly one rule, but {0} were provided")]
        OnlyOneRule(usize),

        #[error("no filters can be configured on opaque routes yet")]
        NoFilters,

        #[error("missing {0}")]
        Missing(&'static str),
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

    impl Tls {
        pub fn fill_backends(&self, set: &mut BackendSet) {
            for Route { ref policy, .. } in &*self.routes {
                policy.distribution.fill_backends(set);
            }
        }
    }

    fn try_route(proto: outbound::TlsRoute) -> Result<Route, InvalidTlsRoute> {
        let outbound::TlsRoute {
            rules,
            snis,
            metadata,
            error,
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

        if rules.len() != 1 {
            // Currently, TLS rules have no match expressions, so if there's
            // more than one rule, we have no way of determining which one to
            // use. Therefore, require that there's exactly one rule.
            return Err(InvalidTlsRoute::OnlyOneRule(rules.len()));
        }

        let policy = rules
            .into_iter()
            .map(|rule| try_rule(&meta, rule, error.clone()))
            .next()
            .ok_or(InvalidTlsRoute::OnlyOneRule(0))??;

        Ok(Route { snis, policy })
    }

    fn try_rule(
        meta: &Arc<Meta>,
        tls_route::Rule { backends }: tls_route::Rule,
        route_error: Option<tls_route::RouteError>,
    ) -> Result<Policy, InvalidTlsRoute> {
        let distribution = backends
            .ok_or(InvalidTlsRoute::Missing("distribution"))?
            .try_into()?;

        let filters = match route_error {
            Some(e) => Arc::new([e.into()]),
            None => NO_FILTERS.clone(),
        };

        Ok(Policy {
            meta: meta.clone(),
            filters,
            params: (),
            distribution,
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
            tls_route::RouteBackend { backend, invalid }: tls_route::RouteBackend,
        ) -> Result<Self, Self::Error> {
            let backend = backend.ok_or(InvalidBackend::Missing("backend"))?;

            let backend = Backend::try_from(backend)?;

            let filters = match invalid {
                Some(invalid) => Arc::new([invalid.into()]),
                None => NO_FILTERS.clone(),
            };

            Ok(RouteBackend { filters, backend })
        }
    }

    impl From<tls_route::RouteError> for Filter {
        fn from(_: tls_route::RouteError) -> Self {
            Self::ForbiddenRoute
        }
    }

    impl From<tls_route::route_backend::Invalid> for Filter {
        fn from(ib: tls_route::route_backend::Invalid) -> Self {
            Self::InvalidBackend(ib.message.into())
        }
    }
}
