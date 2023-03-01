use linkerd_http_route::{grpc, http};
use std::sync::Arc;

pub use linkerd_http_route::grpc::{filter, find, r#match, RouteMatch};

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
    Classify(filter::Classify),
    InternalError(&'static str),
}

pub fn default(distribution: crate::RouteDistribution<Filter>) -> Route {
    Route {
        hosts: vec![],
        rules: vec![Rule {
            matches: vec![],
            policy: Policy {
                meta: crate::Meta::new_default("default"),
                filters: Arc::new([]),
                distribution,
            },
        }],
    }
}

impl Default for Grpc {
    fn default() -> Self {
        Self {
            routes: Arc::new([]),
        }
    }
}
