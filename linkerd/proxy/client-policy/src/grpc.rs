use linkerd_http_route::{grpc, http};
use std::sync::Arc;

pub use linkerd_http_route::grpc::{filter, r#match, RouteMatch};

pub type Policy = crate::RoutePolicy<Filter>;
pub type Route = grpc::Route<Policy>;
pub type Rule = grpc::Rule<Policy>;

// TODO HTTP2 settings
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Grpc {
    pub routes: Arc<[Route]>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Filter {
    InjectFailure(filter::InjectFailure),
    RequestHeaders(http::filter::ModifyHeader),
    InternalError(&'static str),
}

#[inline]
pub fn find<'r, B>(
    routes: &'r [Route],
    req: &::http::Request<B>,
) -> Option<(RouteMatch, &'r Policy)> {
    grpc::find(routes, req)
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
