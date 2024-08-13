use crate::{tcp, Outbound};
use linkerd_app_core::{
    io,
    metrics::prom,
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
    },
    svc,
    tls::ServerName,
    transport::addrs::*,
    Error,
};
use std::{fmt::Debug, hash::Hash};
use tokio::sync::watch;

mod concrete;
mod detect;
mod logical;

pub use self::logical::{Concrete, Routes};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Tls<T> {
    sni: ServerName,
    parent: T,
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

#[derive(Clone, Debug, Default)]
pub struct TlsMetrics {
    balance: concrete::BalancerMetrics,
}

// === impl Outbound ===

impl<C> Outbound<C> {
    /// Builds a stack that proxies tls connections.
    ///
    /// This stack uses caching so that a router/load-balancer may be reused
    /// across multiple connections.
    pub fn push_tls_cached<T, I, R>(self, resolve: R) -> Outbound<svc::ArcNewCloneTcp<T, I>>
    where
        // Tls target
        T: Clone + Debug + PartialEq + Eq + Hash + Send + Sync + 'static,
        T: svc::Param<watch::Receiver<Routes>>,
        // Server-side connection
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + io::Peek,
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
            .push_tls_concrete(resolve)
            .push_tls_logical()
            .map_stack(|config, _rt, stk| {
                stk.push_new_idle_cached(config.discovery_idle_timeout)
                    // Use a dedicated target type to configure parameters for
                    // the TLS stack. It also helps narrow the cache key.
                    .push_map_target(|(sni, parent): (ServerName, T)| Tls { sni, parent })
                    .push(detect::NewDetectSniServer::layer())
                    .arc_new_clone_tcp()
            })
    }
}

// === impl Tls ===

impl<T> svc::Param<ServerName> for Tls<T> {
    fn param(&self) -> ServerName {
        self.sni.clone()
    }
}

impl<T> svc::Param<watch::Receiver<logical::Routes>> for Tls<T>
where
    T: svc::Param<watch::Receiver<logical::Routes>>,
{
    fn param(&self) -> watch::Receiver<logical::Routes> {
        self.parent.param()
    }
}

// === impl TlsMetrics ===

impl TlsMetrics {
    pub fn register(registry: &mut prom::Registry) -> Self {
        let balance =
            concrete::BalancerMetrics::register(registry.sub_registry_with_prefix("balancer"));
        Self { balance }
    }
}
