use linkerd_http_route::http;
use std::sync::Arc;

pub use linkerd_http_route::http::{filter, r#match, RouteMatch};

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

#[inline]
pub fn find<'r, B>(
    routes: &'r [Route],
    req: &::http::Request<B>,
) -> Option<(http::RouteMatch, &'r Policy)> {
    http::find(routes, req)
}

// pub fn default(distribution: crate::RouteDistribution<Filter>) -> Route {
//     Route {
//         hosts: vec![],
//         rules: vec![Rule {
//             matches: vec![],
//             policy: Policy {
//                 meta: crate::Meta::new_default("default"),
//                 filters: Arc::new([]),
//                 distribution,
//             },
//         }],
//     }
// }
