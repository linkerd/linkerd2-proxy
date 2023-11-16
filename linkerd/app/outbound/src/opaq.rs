use crate::{tcp, Outbound};
use linkerd_app_core::{
    io, profiles,
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
    },
    svc,
    transport::addrs::*,
    Error,
};
use std::{fmt::Debug, hash::Hash};

mod concrete;
mod logical;

pub use self::logical::Logical;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Opaq(Logical);

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
            Logical::Route(_, rx) => Some(rx),
            _ => None,
        }
    }
}
