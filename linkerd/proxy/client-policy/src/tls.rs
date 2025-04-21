use linkerd_tls_route as tls;
pub use linkerd_tls_route::{find, sni, RouteMatch};
use std::sync::Arc;

pub type Policy = crate::RoutePolicy<Filter, RouteParams>;
pub type Route = tls::Route<Policy>;

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct RouteParams {
    pub export_hostname_labels: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Tls {
    pub routes: Arc<[Route]>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Filter {
    Forbidden,
    Invalid(Arc<str>),
    InternalError(&'static str),
}

pub fn default(distribution: crate::RouteDistribution<Filter>) -> Route {
    Route {
        snis: vec![],
        policy: Policy {
            meta: crate::Meta::new_default("default"),
            filters: Arc::new([]),
            params: Default::default(),
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
        ClientPolicyOverrides, Meta, RouteBackend, RouteDistribution,
    };
    use linkerd2_proxy_api::outbound::{self, tls_route};
    use linkerd_tls_route::sni::proto::InvalidSniMatch;
    use std::sync::Arc;

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidTlsRoute {
        #[error("invalid sni match: {0}")]
        SniMatch(#[from] InvalidSniMatch),

        #[error("invalid route metadata: {0}")]
        Meta(#[from] InvalidMeta),

        #[error("invalid distribution: {0}")]
        Distribution(#[from] InvalidDistribution),

        #[error("invalid filter: {0}")]
        Filter(#[from] InvalidFilter),

        /// Note: this restriction may be removed in the future, if a way of
        /// actually matching rules for TLS routes is added.
        #[error("a TLS route must have exactly one rule, but {0} were provided")]
        OnlyOneRule(usize),

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

    impl Tls {
        pub fn try_from(
            overrides: ClientPolicyOverrides,
            proto: outbound::proxy_protocol::Tls,
        ) -> Result<Self, InvalidTlsRoute> {
            let routes = proto
                .routes
                .into_iter()
                .map(|p| try_route(p, overrides))
                .collect::<Result<Arc<[_]>, _>>()?;
            Ok(Self { routes })
        }

        pub fn fill_backends(&self, set: &mut BackendSet) {
            for Route { ref policy, .. } in &*self.routes {
                policy.distribution.fill_backends(set);
            }
        }
    }

    fn try_route(
        proto: outbound::TlsRoute,
        overrides: ClientPolicyOverrides,
    ) -> Result<Route, InvalidTlsRoute> {
        let outbound::TlsRoute {
            rules,
            snis,
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

        if rules.len() != 1 {
            // Currently, TLS rules have no match expressions, so if there's
            // more than one rule, we have no way of determining which one to
            // use. Therefore, require that there's exactly one rule.
            return Err(InvalidTlsRoute::OnlyOneRule(rules.len()));
        }

        let policy = rules
            .into_iter()
            .map(|rule| try_rule(&meta, rule, overrides))
            .next()
            .ok_or(InvalidTlsRoute::OnlyOneRule(0))??;

        Ok(Route { snis, policy })
    }

    fn try_rule(
        meta: &Arc<Meta>,
        tls_route::Rule { backends, filters }: tls_route::Rule,
        overrides: ClientPolicyOverrides,
    ) -> Result<Policy, InvalidTlsRoute> {
        let distribution = backends
            .ok_or(InvalidTlsRoute::Missing("distribution"))?
            .try_into()?;

        let filters = filters
            .into_iter()
            .map(Filter::try_from)
            .collect::<Result<Arc<[_]>, _>>()?;

        Ok(Policy {
            meta: meta.clone(),
            filters,
            params: RouteParams::try_from_proto(overrides)?,
            distribution,
        })
    }

    impl RouteParams {
        fn try_from_proto(
            ClientPolicyOverrides {
                export_hostname_labels,
                ..
            }: ClientPolicyOverrides,
        ) -> Result<Self, InvalidTlsRoute> {
            Ok(Self {
                export_hostname_labels,
            })
        }
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
            tls_route::RouteBackend { backend, filters }: tls_route::RouteBackend,
        ) -> Result<RouteBackend<Filter>, InvalidBackend> {
            let backend = backend.ok_or(InvalidBackend::Missing("backend"))?;
            RouteBackend::try_from_proto(backend, filters)
        }
    }

    impl TryFrom<tls_route::Filter> for Filter {
        type Error = InvalidFilter;

        fn try_from(filter: tls_route::Filter) -> Result<Self, Self::Error> {
            use linkerd2_proxy_api::opaque_route::Invalid;
            use tls_route::filter::Kind;

            match filter.kind.ok_or(InvalidFilter::Missing)? {
                Kind::Invalid(Invalid { message }) => Ok(Filter::Invalid(message.into())),
                Kind::Forbidden(_) => Ok(Filter::Forbidden),
            }
        }
    }
}
