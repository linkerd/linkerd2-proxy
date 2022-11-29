use crate::{http, policy, tcp, Outbound};
pub use linkerd_app_core::proxy::api_resolve::ConcreteAddr;
use linkerd_app_core::{
    io, profiles,
    proxy::{api_resolve::Metadata, core::Resolve},
    svc,
    transport::{ClientAddr, Local, OrigDstAddr},
    Addr, Error,
};
pub use profiles::LogicalAddr;
use std::fmt;

#[derive(Clone)]
pub struct Logical<P> {
    pub profile: profiles::Receiver,
    pub policy: Option<policy::Policy>,
    pub logical_addr: LogicalAddr,
    pub orig_dst: OrigDstAddr,
    pub protocol: P,
}

#[derive(Clone, Debug)]
pub struct Concrete<P> {
    pub resolve: ConcreteAddr,
    pub logical: Logical<P>,
}

pub type UnwrapLogical<L, E> = svc::stack::ResultService<svc::Either<L, E>>;

// === impl Logical ===

impl Logical<()> {
    pub(crate) fn new(
        orig_dst: OrigDstAddr,
        logical_addr: LogicalAddr,
        profile: profiles::Receiver,
    ) -> Self {
        Self {
            profile,
            policy: None,
            logical_addr,
            orig_dst,
            protocol: (),
        }
    }
}

/// Used for traffic split.
impl<P> svc::Param<Vec<policy::Backend>> for Logical<P> {
    fn param(&self) -> Vec<policy::Backend> {
        // if a client policy was discovered, use its backends
        if let Some(ref policy) = self.policy {
            return policy.backends();
        }

        self.profile.backends()
    }
}

/// Used for traffic split.
impl<P> svc::Param<policy::split::BackendStream> for Logical<P> {
    fn param(&self) -> policy::split::BackendStream {
        // if a client policy was discovered, use its backends
        if let Some(ref policy) = self.policy {
            return policy.backend_stream();
        }

        self.profile.backend_stream()
    }
}

/// Used for traffic split
impl<P> svc::Param<profiles::Receiver> for Logical<P> {
    fn param(&self) -> profiles::Receiver {
        self.profile.clone()
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

// Used for client policy discovery
impl<P> svc::Param<OrigDstAddr> for Logical<P> {
    fn param(&self) -> OrigDstAddr {
        self.orig_dst
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
            .field("policy", &self.policy)
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
    pub fn push_logical<R, I, P>(
        self,
        resolve: R,
        policies: P,
    ) -> Outbound<svc::ArcNewTcp<tcp::Logical, I>>
    where
        Self: Clone + 'static,
        C: Clone + Send + Sync + Unpin + 'static,
        C: svc::MakeConnection<tcp::Connect, Metadata = Local<ClientAddr>, Error = io::Error>,
        C::Connection: Send + Unpin,
        C::Future: Send + Unpin,
        R: Clone + Send + 'static,
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error> + Sync,
        R::Resolution: Send,
        R::Future: Send + Unpin,
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr,
        I: fmt::Debug + Send + Sync + Unpin + 'static,
        P: svc::Service<OrigDstAddr, Response = policy::Receiver>,
        P: Clone + Send + Sync + 'static,
        P::Future: Send,
        Error: From<P::Error>,
    {
        let http = self
            .clone()
            .push_tcp_endpoint()
            .push_http_endpoint()
            .push_http_logical(resolve.clone(), policies)
            .push_http_server()
            .into_inner();

        self.push_tcp_endpoint()
            .push_tcp_logical(resolve)
            .push_detect_http(http)
    }
}
