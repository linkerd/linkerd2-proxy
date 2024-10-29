use linkerd_tls_route as tls;
use std::sync::Arc;

pub use linkerd_tls_route::{find, sni, RouteMatch};

pub type Policy = crate::RoutePolicy<Filter, ()>;
pub type Route = tls::Route<Policy>;
pub type Rule = tls::Rule<Policy>;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Tls {
    pub routes: Arc<[Route]>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Filter {}

pub fn default(distribution: crate::RouteDistribution<Filter>) -> Route {
    Route {
        snis: vec![],
        rules: vec![Rule {
            matches: vec![],
            policy: Policy {
                meta: crate::Meta::new_default("default"),
                filters: Arc::new([]),
                params: (),
                distribution,
            },
        }],
        forbidden: false,
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
pub mod proto {
    use super::*;
    use crate::proto::BackendSet;

    impl Tls {
        pub fn fill_backends(&self, set: &mut BackendSet) {
            for Route { ref rules, .. } in &*self.routes {
                for Rule { ref policy, .. } in rules {
                    policy.distribution.fill_backends(set);
                }
            }
        }
    }
}
