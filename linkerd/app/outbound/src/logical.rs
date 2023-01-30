use crate::{http, tcp, Outbound};
pub use linkerd_app_core::proxy::api_resolve::ConcreteAddr;
use linkerd_app_core::{
    io, profiles,
    proxy::{api_resolve::Metadata, core::Resolve},
    svc,
    transport::{ClientAddr, Local},
    Addr, Error,
};
pub use profiles::LogicalAddr;
use std::fmt;
use tracing::info_span;

#[derive(Clone)]
pub struct Logical<P> {
    pub profile: profiles::Receiver,
    pub logical_addr: LogicalAddr,
    pub protocol: P,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Concrete<P> {
    pub resolve: ConcreteAddr,
    pub logical: Logical<P>,
}

pub type UnwrapLogical<L, E> = svc::stack::ResultService<svc::Either<L, E>>;

// === impl Logical ===

impl Logical<()> {
    pub(crate) fn new(logical_addr: LogicalAddr, profile: profiles::Receiver) -> Self {
        Self {
            profile,
            logical_addr,
            protocol: (),
        }
    }
}

impl<P> svc::Param<tokio::sync::watch::Receiver<profiles::Profile>> for Logical<P> {
    fn param(&self) -> tokio::sync::watch::Receiver<profiles::Profile> {
        self.profile.clone().into()
    }
}

/// Used for default traffic split
impl<P> svc::Param<profiles::LookupAddr> for Logical<P> {
    fn param(&self) -> profiles::LookupAddr {
        profiles::LookupAddr(self.addr())
    }
}

impl<P> svc::Param<LogicalAddr> for Logical<P> {
    fn param(&self) -> LogicalAddr {
        self.logical_addr.clone()
    }
}

// Used for skipping HTTP detection
impl svc::Param<Option<http::detect::Skip>> for Logical<()> {
    fn param(&self) -> Option<http::detect::Skip> {
        if self.profile.is_opaque_protocol() {
            Some(http::detect::Skip)
        } else {
            None
        }
    }
}

impl svc::Param<Option<http::Version>> for Logical<()> {
    fn param(&self) -> Option<http::Version> {
        None
    }
}

impl<P> Logical<P> {
    pub fn addr(&self) -> Addr {
        Addr::from(self.logical_addr.clone().0)
    }
}

impl<P: PartialEq> PartialEq<Logical<P>> for Logical<P> {
    fn eq(&self, other: &Logical<P>) -> bool {
        self.logical_addr == other.logical_addr && self.protocol == other.protocol
    }
}

impl<P: Eq> Eq for Logical<P> {}

impl<P: std::hash::Hash> std::hash::Hash for Logical<P> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.logical_addr.hash(state);
        self.protocol.hash(state);
    }
}

impl<P: std::fmt::Debug> std::fmt::Debug for Logical<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Logical")
            .field("protocol", &self.protocol)
            .field("profile", &format_args!(".."))
            .field("logical_addr", &self.logical_addr)
            .finish()
    }
}

// === impl Concrete ===

impl<P> From<(ConcreteAddr, Logical<P>)> for Concrete<P> {
    fn from((resolve, logical): (ConcreteAddr, Logical<P>)) -> Self {
        Self { resolve, logical }
    }
}

impl<P> svc::Param<ConcreteAddr> for Concrete<P> {
    fn param(&self) -> ConcreteAddr {
        self.resolve.clone()
    }
}

// === impl Outbound ===

impl<C> Outbound<C> {
    /// Builds a stack that handles protocol detection as well as routing and
    /// load balancing for a single logical destination.
    ///
    /// This stack uses caching so that a router/load-balancer may be reused
    /// across multiple connections.
    pub fn push_logical<R, I>(self, resolve: R) -> Outbound<svc::ArcNewTcp<tcp::Logical, I>>
    where
        Self: Clone + 'static,
        C: Clone + Send + Sync + Unpin + 'static,
        C: svc::MakeConnection<tcp::Connect, Metadata = Local<ClientAddr>, Error = io::Error>,
        C::Connection: Send + Unpin,
        C::Future: Send + Unpin,
        R: Clone + Send + Sync + Unpin + 'static,
        R: Resolve<tcp::Concrete, Endpoint = Metadata, Error = Error>,
        <R as Resolve<tcp::Concrete>>::Resolution: Send,
        <R as Resolve<tcp::Concrete>>::Future: Send + Unpin,
        R: Resolve<http::Concrete, Endpoint = Metadata, Error = Error>,
        <R as Resolve<http::Concrete>>::Resolution: Send,
        <R as Resolve<http::Concrete>>::Future: Send + Unpin,
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr,
        I: fmt::Debug + Send + Sync + Unpin + 'static,
    {
        let http = self
            .clone()
            .push_tcp_endpoint()
            .push_http_endpoint()
            .push_http_concrete(resolve.clone())
            .push_http_logical()
            .push_http_server()
            // The detect stack doesn't cache its inner service, so we need a
            // process-global cache of logical HTTP stacks.
            .map_stack(|config, _, stk| {
                stk.push_new_idle_cached(config.discovery_idle_timeout)
                    .push_on_service(
                        svc::layers()
                            .push(http::Retain::layer())
                            .push(http::BoxResponse::layer()),
                    )
            })
            .into_inner();

        let opaque = self
            .push_tcp_endpoint()
            .push_tcp_concrete(resolve)
            .push_tcp_logical()
            // The detect stack doesn't cache its inner service, so we need a
            // process-global cache of logical TCP stacks.
            .map_stack(|config, _, stk| stk.push_new_idle_cached(config.discovery_idle_timeout));

        opaque.push_detect_http(http).map_stack(|_, _, stk| {
            stk.instrument(|l: &tcp::Logical| info_span!("logical",  svc = %l.logical_addr))
                .push_on_service(svc::BoxService::layer())
                .push(svc::ArcNewService::layer())
        })
    }
}
