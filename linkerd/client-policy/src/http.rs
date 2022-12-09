use crate::RoutePolicy;
use linkerd_http_route::http;
use once_cell::sync::Lazy;
use std::sync::Arc;

pub use linkerd_http_route::http::{filter, r#match, MatchRequest, RouteMatch};

mod router;
pub use self::router::*;

pub type Route = http::Route<RoutePolicy>;
pub type Rule = http::Rule<RoutePolicy>;

#[inline]
pub fn find<'r, B>(
    routes: &'r [Route],
    req: &::http::Request<B>,
) -> Option<(http::RouteMatch, &'r RoutePolicy)> {
    http::find(routes, req)
}

pub(super) static NO_ROUTES: Lazy<Arc<[Route]>> = Lazy::new(|| vec![].into());

pub(super) static DEFAULT_ROUTES: Lazy<Arc<[Route]>> = Lazy::new(|| {
    vec![Route {
        hosts: vec![],
        rules: vec![Rule {
            matches: vec![MatchRequest {
                path: Some(r#match::MatchPath::Prefix(String::from("/"))),
                ..Default::default()
            }],
            policy: RoutePolicy {
                backends: vec![],
                meta: crate::Meta::new_default("default"),
            },
        }],
    }]
    .into()
});

#[cfg(feature = "proto")]
pub mod proto {
    use super::*;
    use crate::{meta::proto::InvalidMeta, proto::InvalidBackend, split::Backend, Meta};
    use linkerd2_proxy_api::outbound as api;
    use linkerd_http_route::http::r#match::{
        host::proto::InvalidHostMatch, proto::InvalidRouteMatch,
    };
    use std::sync::Arc;

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidHttpRoute {
        #[error("invalid host match: {0}")]
        HostMatch(#[from] InvalidHostMatch),

        #[error("invalid route match: {0}")]
        RouteMatch(#[from] InvalidRouteMatch),
        #[error("invalid labels: {0}")]
        Meta(#[from] InvalidMeta),
        #[error("invalid backend: {0}")]
        Backend(#[from] InvalidBackend),
    }

    pub fn try_route(proto: api::HttpRoute) -> Result<Route, InvalidHttpRoute> {
        let api::HttpRoute {
            hosts,
            rules,
            metadata,
        } = proto;

        let hosts = hosts
            .into_iter()
            .map(r#match::MatchHost::try_from)
            .collect::<Result<Vec<_>, InvalidHostMatch>>()?;

        let meta = Arc::new(Meta::try_from(metadata.ok_or(InvalidMeta::Missing)?)?);
        let rules = rules
            .into_iter()
            .map(|r| try_rule(meta.clone(), r))
            .collect::<Result<Vec<_>, InvalidHttpRoute>>()?;

        Ok(Route { hosts, rules })
    }

    fn try_rule(meta: Arc<Meta>, proto: api::http_route::Rule) -> Result<Rule, InvalidHttpRoute> {
        let api::http_route::Rule { matches, backends } = proto;
        let matches = matches
            .into_iter()
            .map(r#match::MatchRequest::try_from)
            .collect::<Result<Vec<_>, InvalidRouteMatch>>()?;
        let backends = backends
            .into_iter()
            .map(Backend::try_from)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Rule {
            matches,
            policy: RoutePolicy { backends, meta },
        })
    }
}
