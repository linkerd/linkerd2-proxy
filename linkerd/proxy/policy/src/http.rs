use linkerd_http_route::http;
pub use linkerd_http_route::http::{filter, r#match, RouteMatch};

pub type Policy<D> = crate::RoutePolicy<Filter, D>;
pub type Route<D> = http::Route<Policy<D>>;
pub type Rule<D> = http::Rule<Policy<D>>;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Filter {
    InjectFailure(filter::InjectFailure),
    Redirect(filter::RedirectRequest),
    RequestHeaders(filter::ModifyHeader),
    InternalError(&'static str),
}

#[inline]
pub fn find<'r, B, D>(
    routes: &'r [Route<D>],
    req: &::http::Request<B>,
) -> Option<(http::RouteMatch, &'r Policy<D>)> {
    http::find(routes, req)
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
    use linkerd_http_route::http::{
        filter::{
            inject_failure::proto::InvalidFailureResponse,
            modify_header::proto::InvalidModifyHeader, redirect::proto::InvalidRequestRedirect,
        },
        r#match::{host::proto::InvalidHostMatch, proto::InvalidRouteMatch},
    };
    use std::sync::Arc;

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidHttpRoute {
        #[error("invalid host match: {0}")]
        HostMatch(#[from] InvalidHostMatch),

        #[error("invalid route match: {0}")]
        RouteMatch(#[from] InvalidRouteMatch),

        #[error("invalid request header modifier: {0}")]
        RequestHeaderModifier(#[from] InvalidModifyHeader),

        #[error("invalid request redirect: {0}")]
        Redirect(#[from] InvalidRequestRedirect),

        #[error("invalid error responder: {0}")]
        ErrorRespnder(#[from] InvalidFailureResponse),

        #[error("invalid authorization: {0}")]
        Authz(#[from] InvalidAuthz),

        #[error("invalid labels: {0}")]
        Meta(#[from] InvalidMeta),
    }

    pub fn try_route(
        proto: api::HttpRoute,
        server_authorizations: &[Authorization],
    ) -> Result<Route, InvalidHttpRoute> {
        let api::HttpRoute {
            hosts,
            authorizations,
            rules,
            metadata,
        } = proto;

        let hosts = hosts
            .into_iter()
            .map(r#match::MatchHost::try_from)
            .collect::<Result<Vec<_>, InvalidHostMatch>>()?;

        let authzs = authz::proto::mk_authorizations(authorizations, server_authorizations)?;
        let meta = Arc::new(Meta::try_from(metadata.ok_or(InvalidMeta::Missing)?)?);
        let rules = rules
            .into_iter()
            .map(|r| try_rule(authzs.clone(), meta.clone(), r))
            .collect::<Result<Vec<_>, InvalidHttpRoute>>()?;

        Ok(Route { hosts, rules })
    }

    fn try_rule(
        authorizations: Arc<[authz::Authorization]>,
        meta: Arc<Meta>,
        proto: api::http_route::Rule,
    ) -> Result<Rule, InvalidHttpRoute> {
        let matches = proto
            .matches
            .into_iter()
            .map(r#match::MatchRequest::try_from)
            .collect::<Result<Vec<_>, InvalidRouteMatch>>()?;

        let policy = {
            use api::http_route::filter;

            let filters = proto
                .filters
                .into_iter()
                .map(|f| match f.kind {
                    Some(filter::Kind::RequestHeaderModifier(rhm)) => {
                        Ok(Filter::RequestHeaders(rhm.try_into()?))
                    }
                    Some(filter::Kind::Redirect(rr)) => Ok(Filter::Redirect(rr.try_into()?)),
                    Some(filter::Kind::FailureInjector(rsp)) => {
                        Ok(Filter::InjectFailure(rsp.try_into()?))
                    }
                    None => Ok(Filter::InternalError(
                        "server policy configured with unknown filter",
                    )),
                })
                .collect::<Result<Vec<_>, InvalidHttpRoute>>()?;

            crate::RoutePolicy {
                authorizations,
                filters,
                meta,
            }
        };

        Ok(Rule { matches, policy })
    }
}
