use self::{
    proxy_connection_close::ProxyConnectionClose, require_id_header::NewRequireIdentity,
    strip_proxy_error::NewStripProxyError,
};
use crate::Outbound;
use linkerd_app_core::{
    profiles,
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

pub mod concrete;
mod endpoint;
pub mod logical;
mod proxy_connection_close;
mod require_id_header;
mod retry;
mod server;
mod strip_proxy_error;

pub use self::logical::{profile, LogicalAddr, Routes};
pub(crate) use self::require_id_header::IdentityRequired;
pub use linkerd_app_core::proxy::http::{self as http, *};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Http<T>(T);

pub fn spawn_routes(
    mut profile_rx: watch::Receiver<profiles::Profile>,
    mut mk: impl FnMut(&profiles::Profile) -> Routes + Send + Sync + 'static,
) -> watch::Receiver<Routes> {
    let (tx, rx) = {
        let routes = (mk)(&*profile_rx.borrow());
        watch::channel(routes)
    };

    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = tx.closed() => return,
                _ = profile_rx.changed() => {
                    let routes = (mk)(&*profile_rx.borrow());
                    if tx.send(routes).is_err() {
                        return;
                    }
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
    pub fn push_http_cached<T, R, NSvc>(
        self,
        resolve: R,
    ) -> Outbound<
        svc::ArcNewService<
            T,
            impl svc::Service<
                    http::Request<http::BoxBody>,
                    Response = http::Response<http::BoxBody>,
                    Error = Error,
                    Future = impl Send,
                > + Clone,
        >,
    >
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
