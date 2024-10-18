use crate::{tcp, Outbound};
use linkerd_app_core::{
    io,
    metrics::prom,
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

mod concrete;
mod logical;

pub use self::logical::{Logical, PolicyRoutes, ProfileRoutes, Routes};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Opaq(Logical);

#[derive(Clone, Debug, Default)]
pub struct OpaqMetrics {
    balance: concrete::BalancerMetrics,
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

impl<C> Outbound<C> {
    /// Builds a stack that proxies opaque TCP connections.
    ///
    /// This stack uses caching so that a router/load-balancer may be reused
    /// across multiple connections.
    pub fn push_opaq_cached<T, I, R>(self, resolve: R) -> Outbound<svc::ArcNewCloneTcp<T, I>>
    where
        // Opaque target
        T: svc::Param<Logical>,
        T: Clone + Send + Sync + 'static,
        // Server-side connection
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr,
        I: Debug + Send + Sync + Unpin + 'static,
        // Endpoint discovery
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>,
        R::Resolution: Unpin,
        // TCP endpoint stack.
        C: svc::MakeConnection<tcp::Connect, Metadata = Local<ClientAddr>, Error = io::Error>,
        C: Clone + Send + Sync + Unpin + 'static,
        C::Connection: Send + Unpin,
        C::Future: Send + Unpin,
    {
        self.push_tcp_endpoint()
            .push_opaq_concrete(resolve)
            .push_opaq_logical()
            .map_stack(|config, _rt, stk| {
                stk.push_new_idle_cached(config.discovery_idle_timeout)
                    // Use a dedicated target type to configure parameters for
                    // the opaque stack. It also helps narrow the cache key.
                    .push_map_target(|t: T| Opaq(t.param()))
                    .arc_new_clone_tcp()
            })
    }
}

// === impl Opaq ===

impl svc::Param<Logical> for Opaq {
    fn param(&self) -> Logical {
        self.0.clone()
    }
}

impl svc::Param<Option<profiles::Receiver>> for Opaq {
    fn param(&self) -> Option<profiles::Receiver> {
        match self.0.param() {
            Logical::Profile(_, rx) => Some(rx),
            _ => None,
        }
    }
}

// === impl OpaqMetrics ===

impl OpaqMetrics {
    pub fn register(registry: &mut prom::Registry) -> Self {
        let balance =
            concrete::BalancerMetrics::register(registry.sub_registry_with_prefix("balancer"));
        Self { balance }
    }
}
