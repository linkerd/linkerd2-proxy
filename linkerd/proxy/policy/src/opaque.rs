use linkerd_http_route::Rule;
pub type Policy<D> = crate::RoutePolicy<Filter, D>;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Filter {}

// TODO(eliza): how would we match opaque routes?
pub type RouteMatch = ();

/// Groups routing rules under a common set of hostnames.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct Route<D> {
    /// Must not be empty.
    pub rules: Vec<Rule<RouteMatch, Policy<D>>>,
}

pub fn default<D: Default>(authorizations: std::sync::Arc<[crate::Authorization]>) -> Route<D> {
    Route {
        rules: vec![Rule {
            matches: vec![],
            policy: Policy {
                meta: crate::Meta::new_default("default"),
                authorizations,
                filters: vec![],
                distribution: D::default(),
            },
        }],
    }
}
