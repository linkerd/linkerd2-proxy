use super::concrete;
use crate::{BackendRef, Outbound, ParentRef};
use linkerd_app_core::{io, svc, tls::ServerName, Addr, Error};
use linkerd_proxy_client_policy as client_policy;
use std::{fmt::Debug, hash::Hash, sync::Arc};
use tokio::sync::watch;

pub mod route;
pub mod router;

#[cfg(test)]
mod tests;

/// Indicates the address used for logical routing.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LogicalAddr(pub Addr);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Routes {
    pub addr: Addr,
    pub meta: ParentRef,
    pub routes: Arc<[client_policy::tls::Route]>,
    pub backends: Arc<[client_policy::Backend]>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Concrete<T> {
    target: concrete::Dispatch,
    parent: T,
    parent_ref: ParentRef,
    backend_ref: BackendRef,
}

#[derive(Debug, thiserror::Error)]
#[error("no route")]
pub struct NoRoute;

#[derive(Debug, thiserror::Error)]
#[error("logical service {addr}: {source}")]
pub struct LogicalError {
    addr: Addr,
    #[source]
    source: Error,
}

impl<N> Outbound<N> {
    /// Builds a `NewService` that produces a router service for each logical
    /// target.
    ///
    /// The router uses discovery information (provided on the target) to
    /// support per-connection routing over a set of concrete inner services.
    /// Only available inner services are used for routing. When there are no
    /// available backends, requests are failed with a [`svc::stack::LoadShedError`].
    pub fn push_tls_logical<T, I, NSvc>(self) -> Outbound<svc::ArcNewCloneTcp<T, I>>
    where
        // Logical target.
        T: svc::Param<watch::Receiver<Routes>>,
        T: svc::Param<ServerName>,
        T: Eq + Hash + Clone + Debug + Send + Sync + 'static,
        // Concrete stack.
        I: io::AsyncRead + io::AsyncWrite + Debug + Send + Unpin + 'static,
        // Concrete stack.
        N: svc::NewService<Concrete<T>, Service = NSvc> + Clone + Send + Sync + 'static,
        NSvc: svc::Service<I, Response = ()> + Clone + Send + Sync + 'static,
        NSvc::Future: Send,
        NSvc::Error: Into<Error>,
    {
        self.map_stack(|_config, _, concrete| {
            concrete
                .lift_new()
                .push_on_service(svc::layer::mk(move |concrete: N| {
                    svc::stack(concrete.clone())
                        .push(router::Router::layer())
                        .push(svc::NewMapErr::layer_from_target::<LogicalError, _>())
                        .arc_new_clone_tcp()
                        .into_inner()
                }))
                // Rebuild the inner router stack every time the watch changes.
                .push(svc::NewSpawnWatch::<Routes, _>::layer_into::<
                    router::Router<T>,
                >())
                .arc_new_clone_tcp()
        })
    }
}

// === impl LogicalError ===

impl<T> From<(&router::Router<T>, Error)> for LogicalError
where
    T: Eq + Hash + Clone + Debug,
{
    fn from((target, source): (&router::Router<T>, Error)) -> Self {
        let LogicalAddr(addr) = svc::Param::param(target);
        Self { addr, source }
    }
}

impl<T> svc::Param<concrete::Dispatch> for Concrete<T> {
    fn param(&self) -> concrete::Dispatch {
        self.target.clone()
    }
}

impl<T> svc::Param<ServerName> for Concrete<T>
where
    T: svc::Param<ServerName>,
{
    fn param(&self) -> ServerName {
        self.parent.param()
    }
}
