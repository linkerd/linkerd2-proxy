use crate::{http, Outbound};
pub use linkerd_app_core::proxy::api_resolve::ConcreteAddr;
use linkerd_app_core::{io, profiles, svc, transport::addrs::*, Addr, Error};
use std::{fmt::Debug, hash::Hash};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Detected<T> {
    version: http::Version,
    parent: T,
}

#[derive(Clone)]
pub struct Logical<P> {
    pub profile: profiles::Receiver,
    pub logical_addr: profiles::LogicalAddr,
    pub protocol: P,
}

pub type UnwrapLogical<L, E> = svc::stack::ResultService<svc::Either<L, E>>;

// === impl Logical ===

impl Logical<()> {
    pub(crate) fn new(logical_addr: profiles::LogicalAddr, profile: profiles::Receiver) -> Self {
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

impl<P> svc::Param<profiles::LogicalAddr> for Logical<P> {
    fn param(&self) -> profiles::LogicalAddr {
        self.logical_addr.clone()
    }
}

impl<P> svc::Param<Option<profiles::LogicalAddr>> for Logical<P> {
    fn param(&self) -> Option<profiles::LogicalAddr> {
        Some(self.logical_addr.clone())
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

impl<P: Debug> Debug for Logical<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Logical")
            .field("protocol", &self.protocol)
            .field("profile", &format_args!(".."))
            .field("logical_addr", &self.logical_addr)
            .finish()
    }
}

// === impl Outbound ===

impl<N> Outbound<N> {
    /// Builds a stack that handles protocol detection as well as routing and
    /// load balancing for a single logical destination.
    ///
    /// This stack uses caching so that a router/load-balancer may be reused
    /// across multiple connections.
    pub fn push_protocol<T, I, H, HSvc, NSvc>(self, http: H) -> Outbound<svc::ArcNewTcp<T, I>>
    where
        T: svc::Param<Option<http::detect::Skip>>,
        T: Eq + Hash + Clone + Debug + Send + Sync + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr,
        I: Debug + Send + Sync + Unpin + 'static,
        H: svc::NewService<Detected<T>, Service = HSvc>,
        H: Clone + Send + Sync + Unpin + 'static,
        HSvc: svc::Service<
            http::Request<http::BoxBody>,
            Response = http::Response<http::BoxBody>,
            Error = Error,
        >,
        HSvc: Clone + Send + Sync + Unpin + 'static,
        HSvc::Future: Send,
        N: svc::NewService<T, Service = NSvc>,
        N: Clone + Send + Sync + Unpin + 'static,
        NSvc: svc::Service<io::EitherIo<I, io::PrefixedIo<I>>, Response = (), Error = Error>,
        NSvc: Clone + Send + Sync + Unpin + 'static,
        NSvc::Future: Send,
    {
        let opaque = self.clone().map_stack(|config, _, opaque| {
            // The detect stack doesn't cache its inner service, so we need a
            // process-global cache of logical TCP stacks.
            opaque
                .push_new_idle_cached(config.discovery_idle_timeout)
                .check_new_service::<T, _>()
        });

        let http = self
            .with_stack(http)
            .map_stack(|config, _, stk| {
                stk.push_new_idle_cached(config.discovery_idle_timeout)
                    .push_on_service(
                        svc::layers()
                            .push(http::Retain::layer())
                            .push(http::BoxResponse::layer()),
                    )
            })
            .check_new_service::<Detected<T>, http::Request<_>>();

        opaque
            .push_detect_http(http.into_inner())
            .map_stack(|_, _, stk| {
                stk.push_on_service(svc::BoxService::layer())
                    .push(svc::ArcNewService::layer())
            })
    }
}

// === impl Detected ===

impl<T> From<(http::Version, T)> for Detected<T> {
    fn from((version, parent): (http::Version, T)) -> Self {
        Self { version, parent }
    }
}

impl<T> svc::Param<http::Version> for Detected<T> {
    fn param(&self) -> http::Version {
        self.version
    }
}

impl<T> svc::Param<Remote<ServerAddr>> for Detected<T>
where
    T: svc::Param<Remote<ServerAddr>>,
{
    fn param(&self) -> Remote<ServerAddr> {
        self.parent.param()
    }
}

impl<T> svc::Param<http::normalize_uri::DefaultAuthority> for Detected<T>
where
    T: svc::Param<http::normalize_uri::DefaultAuthority>,
{
    fn param(&self) -> http::normalize_uri::DefaultAuthority {
        self.parent.param()
    }
}

impl<T> svc::Param<Option<profiles::LogicalAddr>> for Detected<T>
where
    T: svc::Param<Option<profiles::LogicalAddr>>,
{
    fn param(&self) -> Option<profiles::LogicalAddr> {
        self.parent.param()
    }
}

impl<T> svc::Param<Option<profiles::Receiver>> for Detected<T>
where
    T: svc::Param<Option<profiles::Receiver>>,
{
    fn param(&self) -> Option<profiles::Receiver> {
        self.parent.param()
    }
}
