use super::concrete;
use crate::{BackendRef, Outbound, ParentRef};
use linkerd_app_core::{io, svc, Addr, Error};
use linkerd_proxy_client_policy as client_policy;
use std::{fmt::Debug, hash::Hash, sync::Arc};
use tokio::sync::watch;

pub mod route;
pub mod router;

#[cfg(test)]
mod tests;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Logical {
    pub meta: ParentRef,
    pub addr: Addr,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Routes {
    pub logical: Logical,
    pub routes: Option<client_policy::opaq::Route>,
    pub backends: Arc<[client_policy::Backend]>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Concrete<T> {
    target: concrete::Dispatch,
    parent: T,
    logical: Logical,
    backend_ref: BackendRef,
}

#[derive(Debug, thiserror::Error)]
#[error("no route")]
pub struct NoRoute;

#[derive(Debug, thiserror::Error)]
#[error("logical service {0}: {source}", logical.addr)]
pub struct LogicalError {
    logical: Logical,
    #[source]
    source: Error,
}

impl<N> Outbound<N> {
    /// Builds a `NewService` that produces a router service for each logical
    /// target.
    ///
    /// The router uses discovery information (provided on the target) to
    /// support per-request connection routing over a set of concrete inner
    /// services. Only available inner services are used for routing. When
    /// there are no available backends, requests are failed with a
    /// [`svc::stack::LoadShedError`].
    pub fn push_opaq_logical<T, I, NSvc>(self) -> Outbound<svc::ArcNewCloneTcp<T, I>>
    where
        // Opaque logical target.
        T: svc::Param<watch::Receiver<Routes>>,
        T: Eq + Hash + Clone + Debug + Send + Sync + 'static,
        // Server-side socket.
        I: io::AsyncRead + io::AsyncWrite + Debug + Send + Unpin + 'static,
        // Concrete stack.
        N: svc::NewService<Concrete<T>, Service = NSvc> + Clone + Send + Sync + 'static,
        NSvc: svc::Service<I, Response = ()> + Clone + Send + Sync + 'static,
        NSvc::Future: Send,
        NSvc::Error: Into<Error>,
    {
        self.map_stack(|_config, rt, concrete| {
            let metrics = rt.metrics.prom.opaq.route.clone();

            concrete
                .lift_new()
                .push_on_service(router::Router::layer(metrics.clone()))
                .push_on_service(svc::NewMapErr::layer_from_target::<LogicalError, _>())
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
        Self {
            logical: target.logical.clone(),
            source,
        }
    }
}

// === impl Concrete ===

impl<T> svc::Param<concrete::Dispatch> for Concrete<T> {
    fn param(&self) -> concrete::Dispatch {
        self.target.clone()
    }
}

impl<T> svc::Param<Logical> for Concrete<T> {
    fn param(&self) -> Logical {
        self.logical.clone()
    }
}

impl<T> svc::Param<ParentRef> for Concrete<T> {
    fn param(&self) -> ParentRef {
        self.logical.meta.clone()
    }
}

impl<T> svc::Param<BackendRef> for Concrete<T> {
    fn param(&self) -> BackendRef {
        self.backend_ref.clone()
    }
}
