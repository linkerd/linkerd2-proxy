use super::{Route, RoutePolicy};
use std::{hash::Hash, sync::Arc};

mod proxy;
mod service;
pub use self::{proxy::NewProxyRouter, service::NewServiceRouter};

pub trait FindRoute {
    type Route: Clone + Hash + Eq;

    fn with_routes<F, T>(&self, f: F) -> T
    where
        F: FnOnce(&mut dyn Iterator<Item = Self::Route>) -> T;

    fn find_route<'r, B>(&'r self, request: &http::Request<B>) -> Option<&'r Self::Route>;
}

// === impl FindRoute ===

impl FindRoute for Arc<[Route]> {
    type Route = RoutePolicy;

    fn with_routes<F, T>(&self, f: F) -> T
    where
        F: FnOnce(&mut dyn Iterator<Item = Self::Route>) -> T,
    {
        let mut iter = self
            .iter()
            .flat_map(|route| route.rules.iter())
            .map(|rule| rule.policy.clone());
        f(&mut iter)
    }

    fn find_route<'r, B>(&'r self, request: &http::Request<B>) -> Option<&'r Self::Route> {
        match super::find(self, request) {
            Some((req_match, policy)) => {
                tracing::trace!(
                    route.group = %policy.meta.group(),
                    route.kind = %policy.meta.kind(),
                    route.name = %policy.meta.name(),
                    "req.match" = ?req_match,
                    "Using HTTPRoute service",
                );
                Some(policy)
            }
            None => {
                tracing::warn!("No HTTPRoutes matched");
                None
            }
        }
    }
}
