use self::require_id_header::NewRequireIdentity;
use crate::Outbound;
use linkerd_app_core::{
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
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
mod server;

pub use self::logical::{policy, profile, LogicalAddr, Routes};
pub(crate) use self::require_id_header::IdentityRequired;
pub use linkerd_app_core::proxy::http::{self as http, *};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Http<T>(T);

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

impl<N> Outbound<N> {
    /// Builds a stack that routes HTTP requests to endpoint stacks.
    ///
    /// Buffered concrete services are cached in and evicted when idle.
    pub fn push_http_cached<T, R, NSvc>(self, resolve: R) -> Outbound<svc::ArcNewCloneHttp<T>>
    where
        // Logical HTTP target.
        T: svc::Param<http::Version>,
        T: svc::Param<watch::Receiver<Routes>>,
        T: Clone + Debug + PartialEq + Eq + Hash + Send + Sync + 'static,
        // Endpoint resolution.
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>,
        // HTTP client stack
        N: svc::NewService<concrete::Endpoint<logical::Concrete<Http<T>>>, Service = NSvc>,
        N: Clone + Send + Sync + Unpin + 'static,
        NSvc: svc::Service<
            http::Request<http::BoxBody>,
            Response = http::Response<http::BoxBody>,
            Error = Error,
        >,
        NSvc: Send + 'static,
        NSvc::Future: Send + Unpin + 'static,
    {
        self.push_http_endpoint()
            .push_http_concrete(resolve)
            .push_http_logical()
            .map_stack(move |config, _, stk| {
                stk.push_new_idle_cached(config.discovery_idle_timeout)
                    .push_map_target(Http)
                    .push_on_service(svc::BoxCloneService::layer())
                    .push(svc::ArcNewService::layer())
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
