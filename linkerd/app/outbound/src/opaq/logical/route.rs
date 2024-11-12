use super::{super::Concrete, Logical};
use crate::RouteRef;
use linkerd_app_core::{io, svc, Error};
use linkerd_distribute as distribute;
use linkerd_proxy_client_policy as policy;
use std::{fmt::Debug, hash::Hash, sync::Arc};

mod filters;

#[derive(Debug, PartialEq, Eq, Hash)]
pub(crate) struct Backend<T> {
    pub(crate) route_ref: RouteRef,
    pub(crate) concrete: Concrete<T>,
    pub(super) filters: Arc<[policy::opaq::Filter]>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct MatchedRoute<T> {
    pub(super) params: Route<T>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Route<T> {
    pub(super) parent: T,
    pub(super) logical: Logical,
    pub(super) route_ref: RouteRef,
    pub(super) filters: Arc<[policy::opaq::Filter]>,
    pub(super) distribution: BackendDistribution<T>,
}

pub(crate) type BackendDistribution<T> = distribute::Distribution<Backend<T>>;
pub(crate) type NewDistribute<T, N> = distribute::NewDistribute<Backend<T>, (), N>;

/// Wraps errors with route metadata.
#[derive(Debug, thiserror::Error)]
#[error("route {}: {source}", route.0)]
struct RouteError {
    route: RouteRef,
    #[source]
    source: Error,
}

// === impl Backend ===

impl<T: Clone> Clone for Backend<T> {
    fn clone(&self) -> Self {
        Self {
            route_ref: self.route_ref.clone(),
            concrete: self.concrete.clone(),
            filters: self.filters.clone(),
        }
    }
}

// === impl MatchedRoute ===

impl<T> MatchedRoute<T>
where
    // Parent target.
    T: Debug + Eq + Hash,
    T: Clone + Send + Sync + 'static,
{
    /// Builds a route stack that applies policy filters to requests and
    /// distributes requests over each route's backends. These [`Concrete`]
    /// backends are expected to be cached/shared by the inner stack.
    pub(crate) fn layer<N, I, NSvc>(
    ) -> impl svc::Layer<N, Service = svc::ArcNewCloneTcp<Self, I>> + Clone
    where
        I: io::AsyncRead + io::AsyncWrite + Debug + Send + Unpin + 'static,
        // Inner stack.
        N: svc::NewService<Concrete<T>, Service = NSvc> + Clone + Send + Sync + 'static,
        NSvc: svc::Service<I, Response = ()> + Clone + Send + Sync + 'static,
        NSvc::Future: Send,
        NSvc::Error: Into<Error>,
    {
        svc::layer::mk(move |inner| {
            svc::stack(inner)
                .push_map_target(|t| t)
                .push_map_target(|b: Backend<T>| b.concrete)
                // apply backend filters
                .push_filter(filters::apply)
                .lift_new()
                .push(NewDistribute::layer())
                // The router does not take the backend's availability into
                // consideration, so we must eagerly fail requests to prevent
                // leaking tasks onto the runtime.
                .push_on_service(svc::LoadShed::layer())
                // apply route level filters
                .push_filter(filters::apply)
                .push(svc::NewMapErr::layer_with(|rt: &Self| {
                    let route = rt.params.route_ref.clone();
                    move |source| RouteError {
                        route: route.clone(),
                        source,
                    }
                }))
                .arc_new_clone_tcp()
                .into_inner()
        })
    }
}

impl<T: Clone> svc::Param<BackendDistribution<T>> for MatchedRoute<T> {
    fn param(&self) -> BackendDistribution<T> {
        self.params.distribution.clone()
    }
}

impl<T: Clone> svc::Param<Arc<[policy::opaq::Filter]>> for MatchedRoute<T> {
    fn param(&self) -> Arc<[policy::opaq::Filter]> {
        self.params.filters.clone()
    }
}

impl<T: Clone> svc::Param<Arc<[policy::opaq::Filter]>> for Backend<T> {
    fn param(&self) -> Arc<[policy::opaq::Filter]> {
        self.filters.clone()
    }
}
