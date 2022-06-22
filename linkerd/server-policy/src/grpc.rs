use linkerd_http_route::grpc;
pub use linkerd_http_route::grpc::r#match;

pub type Policy = crate::RoutePolicy;
pub type Route = grpc::Route<Policy>;
pub type Rule = grpc::Rule<Policy>;

#[inline]
pub fn find<'r, B>(
    routes: &'r [Route],
    req: &::http::Request<B>,
) -> Option<(grpc::RouteMatch, &'r Policy)> {
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
            },
        }],
    }
}
