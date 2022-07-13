pub use linkerd_http_route::grpc::{filter, r#match, RouteMatch};
use linkerd_http_route::{grpc, http};

pub type Policy = crate::RoutePolicy<Filter>;
pub type Route = grpc::Route<Policy>;
pub type Rule = grpc::Rule<Policy>;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Filter {
    InjectFailure(filter::InjectFailure),
    RequestHeaders(http::filter::ModifyHeader),
    Unknown,
}

#[inline]
pub fn find<'r, B>(
    routes: &'r [Route],
    req: &::http::Request<B>,
) -> Option<(RouteMatch, &'r Policy)> {
    grpc::find(routes, req)
}

pub fn default(authorizations: std::sync::Arc<[crate::Authorization]>) -> Route {
    Route {
        hosts: vec![],
        rules: vec![Rule {
            matches: vec![],
            policy: Policy {
                meta: crate::Meta::new_default("default"),
                authorizations,
                filters: vec![],
            },
        }],
    }
}

#[cfg(feature = "proto")]
pub mod proto {
    use super::*;
    use crate::{
        authz::{self, proto::InvalidAuthz},
        meta::proto::InvalidMeta,
        Authorization, Meta,
    };
    use linkerd2_proxy_api::inbound as api;
    use linkerd_http_route::{
        grpc::{
            filter::inject_failure::proto::InvalidFailureResponse,
            r#match::proto::InvalidRouteMatch,
        },
        http::{
            filter::modify_header::proto::InvalidModifyHeader,
            r#match::host::proto::InvalidHostMatch,
        },
    };
    use std::sync::Arc;

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidGrpcRoute {
        #[error("invalid host match: {0}")]
        HostMatch(#[from] InvalidHostMatch),

        #[error("invalid route match: {0}")]
        RouteMatch(#[from] InvalidRouteMatch),

        #[error("invalid request header modifier: {0}")]
        RequestHeaderModifier(#[from] InvalidModifyHeader),

        #[error("invalid error responder: {0}")]
        ErrorRespnder(#[from] InvalidFailureResponse),

        #[error("invalid authorization: {0}")]
        Authz(#[from] InvalidAuthz),

        #[error("invalid metadata: {0}")]
        Meta(#[from] InvalidMeta),
    }

    pub fn try_route(
        proto: api::GrpcRoute,
        server_authorizations: &[Authorization],
    ) -> Result<Route, InvalidGrpcRoute> {
        let api::GrpcRoute {
            hosts,
            authorizations,
            rules,
            metadata,
        } = proto;

        let hosts = hosts
            .into_iter()
            .map(http::r#match::MatchHost::try_from)
            .collect::<Result<Vec<_>, InvalidHostMatch>>()?;

        let authzs = authz::proto::mk_authorizations(authorizations, server_authorizations)?;
        let meta = Arc::new(Meta::try_from(metadata.ok_or(InvalidMeta::Missing)?)?);
        let rules = rules
            .into_iter()
            .map(|r| try_rule(authzs.clone(), meta.clone(), r))
            .collect::<Result<Vec<_>, InvalidGrpcRoute>>()?;

        Ok(Route { hosts, rules })
    }

    fn try_rule(
        authorizations: Arc<[authz::Authorization]>,
        meta: Arc<Meta>,
        proto: api::grpc_route::Rule,
    ) -> Result<Rule, InvalidGrpcRoute> {
        let matches = proto
            .matches
            .into_iter()
            .map(r#match::MatchRoute::try_from)
            .collect::<Result<Vec<_>, InvalidRouteMatch>>()?;

        let policy = {
            use api::grpc_route::filter;

            let filters = proto
                .filters
                .into_iter()
                .map(|f| match f.kind {
                    Some(filter::Kind::FailureInjector(rsp)) => {
                        Ok(Filter::InjectFailure(rsp.try_into()?))
                    }
                    Some(filter::Kind::RequestHeaderModifier(rhm)) => {
                        Ok(Filter::RequestHeaders(rhm.try_into()?))
                    }
                    None => Ok(Filter::Unknown),
                })
                .collect::<Result<Vec<_>, InvalidGrpcRoute>>()?;

            crate::RoutePolicy {
                authorizations,
                filters,
                meta,
            }
        };

        Ok(Rule { matches, policy })
    }
}
