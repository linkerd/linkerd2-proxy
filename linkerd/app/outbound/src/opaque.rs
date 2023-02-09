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

pub mod concrete;
pub mod forward;
pub mod logical;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Opaq(logical::Target);

// === impl Outbound ===

impl<C> Outbound<C> {
    /// Builds a stack that handles protocol detection as well as routing and
    /// load balancing for a single logical destination.
    ///
    /// This stack uses caching so that a router/load-balancer may be reused
    /// across multiple connections.
    pub fn push_opaque<T, I, R>(
        self,
        resolve: R,
    ) -> Outbound<
        svc::ArcNewService<
            T,
            impl svc::Service<I, Response = (), Error = Error, Future = impl Send> + Clone,
        >,
    >
    where
        // Target
        T: svc::Param<logical::Target>,
        T: Eq + Hash + Clone + Debug + Send + Sync + 'static,
        // Server-side connection
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr,
        I: Debug + Send + Sync + Unpin + 'static,
        // Client-side connector service
        C: svc::MakeConnection<tcp::Connect, Metadata = Local<ClientAddr>, Error = io::Error>,
        C: Clone + Send + Sync + Unpin + 'static,
        C::Connection: Send + Unpin,
        C::Future: Send + Unpin,
        // Endpoint discovery
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>,
    {
        self.push_tcp_endpoint()
            .push_opaque_concrete(resolve)
            .push_opaque_logical()
            .map_stack(|_, _, stk| {
                stk.push_map_target(|t: T| Opaq(t.param()))
                    .push(svc::ArcNewService::layer())
            })
    }
}

// === impl Opaq ===

impl svc::Param<logical::Target> for Opaq {
    fn param(&self) -> logical::Target {
        self.0.clone()
    }
}

impl svc::Param<Option<profiles::Receiver>> for Opaq {
    fn param(&self) -> Option<profiles::Receiver> {
        match self.0.param() {
            logical::Target::Route(_, rx) => Some(rx),
            _ => None,
        }
    }
}
