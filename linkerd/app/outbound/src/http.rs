use self::require_id_header::NewRequireIdentity;
use crate::Outbound;
use linkerd_app_core::{
    metrics::prom,
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
        http,
    },
    svc,
    transport::addrs::*,
    Error,
};
use std::{fmt::Debug, hash::Hash};
use tokio::sync::watch;

mod breaker;
pub mod concrete;
mod endpoint;
mod handle_proxy_error_headers;
pub mod logical;
mod require_id_header;
mod retry;
pub(crate) mod server;

pub use self::logical::{policy, profile, LogicalAddr, Routes};
pub(crate) use self::require_id_header::IdentityRequired;
pub use linkerd_app_core::proxy::http::*;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Http<T>(T);

#[derive(Clone, Debug, Default)]
pub struct HttpMetrics {
    balancer: concrete::BalancerMetrics,
    route: policy::RouteMetrics,
}

#[derive(Clone, Debug, Default)]
pub struct GrpcMetrics {
    route: policy::RouteMetrics,
}

pub fn spawn_routes<T>(
    mut route_rx: watch::Receiver<T>,
    init: Routes,
    mut mk: impl FnMut(&T) -> Option<Routes> + Send + Sync + 'static,
) -> watch::Receiver<Routes>
where
    T: Send + Sync + 'static,
{
    let (tx, rx) = watch::channel(init);

    tokio::spawn(async move {
        loop {
            let res = tokio::select! {
                biased;
                _ = tx.closed() => return,
                res = route_rx.changed() => res,
            };

            if res.is_err() {
                // Drop the `tx` sender when the profile sender is
                // dropped.
                return;
            }

            if let Some(routes) = (mk)(&*route_rx.borrow_and_update()) {
                if tx.send(routes).is_err() {
                    // Drop the `tx` sender when all of its receivers are dropped.
                    return;
                }
            }
        }
    });

    rx
}

pub fn spawn_routes_default(addr: Remote<ServerAddr>) -> watch::Receiver<Routes> {
    let (tx, rx) = watch::channel(Routes::Endpoint(addr, Default::default()));
    tokio::spawn(async move {
        tx.closed().await;
    });
    rx
}

// === impl Outbound ===

impl<T> Outbound<svc::ArcNewHttp<concrete::Endpoint<logical::Concrete<Http<T>>>>> {
    /// Builds a stack that routes HTTP requests to endpoint stacks.
    ///
    /// Buffered concrete services are cached in and evicted when idle.
    pub fn push_http_cached<R>(
        self,
        http_metrics: HttpMetrics,
        grpc_metrics: GrpcMetrics,
        resolve: R,
    ) -> Outbound<svc::ArcNewCloneHttp<T>>
    where
        // Logical HTTP target.
        T: svc::Param<http::Version>,
        T: svc::Param<watch::Receiver<Routes>>,
        T: Clone + Debug + PartialEq + Eq + Hash + Send + Sync + 'static,
        // Endpoint resolution.
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>,
        R::Resolution: Unpin,
    {
        self.push_http_endpoint()
            .push_http_concrete(http_metrics.balancer, resolve)
            .push_http_logical(http_metrics.route, grpc_metrics.route)
            .map_stack(move |config, _, stk| {
                stk.push_new_idle_cached(config.discovery_idle_timeout)
                    .push_map_target(Http)
                    .arc_new_clone_http()
            })
    }
}

// === impl Http ===

impl<T> svc::Param<http::Version> for Http<T>
where
    T: svc::Param<http::Version>,
{
    fn param(&self) -> http::Version {
        self.0.param()
    }
}

impl<T> svc::Param<watch::Receiver<Routes>> for Http<T>
where
    T: svc::Param<watch::Receiver<Routes>>,
{
    fn param(&self) -> watch::Receiver<Routes> {
        self.0.param()
    }
}

// === impl HttpMetrics ===

impl HttpMetrics {
    pub fn register(registry: &mut prom::Registry) -> Self {
        // let http = registry.sub_registry_with_prefix("http");
        let route = policy::RouteMetrics::register(registry.sub_registry_with_prefix("route"));
        let balancer =
            concrete::BalancerMetrics::register(registry.sub_registry_with_prefix("balancer"));
        Self { route, balancer }
    }
}

impl GrpcMetrics {
    pub fn register(registry: &mut prom::Registry) -> Self {
        // let grpc = registry.sub_registry_with_prefix("grpc");
        let route = policy::RouteMetrics::register(registry.sub_registry_with_prefix("route"));
        Self { route }
    }
}
