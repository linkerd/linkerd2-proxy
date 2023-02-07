use crate::{tcp, Outbound};
use linkerd_app_core::{
    io, profiles,
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
        tcp::balance,
    },
    svc,
    transport::addrs::*,
    Error, NameAddr,
};
use std::{fmt::Debug, hash::Hash, net::SocketAddr};

pub mod concrete;
pub mod forward;
pub mod logical;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Dispatch {
    Balance(NameAddr, balance::EwmaConfig),
    Forward(SocketAddr, Metadata),
}

// === impl Outbound ===

impl<C> Outbound<C> {
    /// Builds a stack that handles protocol detection as well as routing and
    /// load balancing for a single logical destination.
    ///
    /// This stack uses caching so that a router/load-balancer may be reused
    /// across multiple connections.
    pub fn push_opaque<T, R, I>(
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
        T: svc::Param<Remote<ServerAddr>>,
        // T: svc::Param<Option<profiles::LogicalAddr>>,
        T: svc::Param<Option<profiles::Receiver>>,
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
        R: Clone + Send + Sync + Unpin + 'static,
    {
        self.push_tcp_endpoint()
            .push_opaque_concrete(resolve)
            .push_opaque_logical()
    }
}
