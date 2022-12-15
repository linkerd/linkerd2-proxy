use futures::Stream;
use linkerd_app_core::profiles::http as profiles;
pub use linkerd_client_policy::http::*;
use std::{pin::Pin, sync::Arc};

#[derive(Clone, Debug)]
pub enum RouteList {
    HttpRoute(Arc<[Route]>),
    Profile(profiles::RouteList),
}

pub type RouteListStream = Pin<Box<dyn Stream<Item = RouteList> + Send + 'static>>;

impl FindRoute for RouteList {
    type Route = RoutePolicy;

    fn with_routes<F, T>(&self, f: F) -> T
    where
        F: FnOnce(&mut dyn Iterator<Item = Self::Route>) -> T,
    {
        match self {
            Self::HttpRoute(routes) => routes.with_routes(f),
            Self::Profile(routes) => routes.with_routes(f),
        }
    }

    fn find_route<'r, B>(&'r self, request: &http::Request<B>) -> Option<&'r Self::Route> {
        match self {
            Self::HttpRoute(routes) => routes.find_route(request),
            Self::Profile(routes) => routes.find_route(request),
        }
    }
}
